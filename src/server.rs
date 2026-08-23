use std::{borrow::Cow, sync::Arc, time::Instant};

/// Maximum number of characters included in recall result snippets.
/// Content longer than this is truncated and flagged with `truncated: true`.
const SNIPPET_MAX_CHARS: usize = 500;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        tool::{Extension, ToolCallContext},
        wrapper::Parameters,
    },
    model::{
        CallToolRequestParams, CallToolResult, ErrorData, Meta, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn, Instrument};

/// Extract the `Mcp-Session-Id` header from HTTP request parts.
///
/// Returns `"unknown"` if the header is absent or not valid UTF-8.
/// Truncates to 128 chars to bound span field size from untrusted input.
fn extract_session_id(parts: &http::request::Parts) -> String {
    let raw = parts
        .headers
        .get("mcp-session-id")
        .and_then(|v: &http::HeaderValue| v.to_str().ok())
        .unwrap_or("unknown");
    if raw.len() > 128 {
        let truncated: String = raw.chars().take(128).collect();
        format!("{truncated}…")
    } else {
        raw.to_owned()
    }
}

use crate::{
    config::Config,
    embedding::EmbeddingBackend,
    error::MemoryError,
    federation::{Federation, StoreReadResult, StoreReadStatus},
    index::VectorStore,
    recall_log::{BatchVerdict, RecallLog, RecallResult},
    repo::{traced_spawn_blocking, MemoryRepo},
    repo_router::MultiSyncResult,
    search::{
        bm25::DegradeOnDrop, hybrid_search, spawn_lexical_repair_for_router, FusedHit, LexicalDoc,
        LexicalIndex, LexicalOp,
    },
    types::{
        parse_qualified_name, AppState, BatchMarkAppliedArgs, EditArgs, ForgetArgs, ListField,
        ListToolArgs, MarkAppliedArgs, Memory, MemoryMetadata, MemoryName, MemoryRef, MoveArgs,
        PullResult, ReadToolArgs, RecallArgs, RecallStatsArgs, ReindexStats, RememberArgs,
        ResolvedChanges, Scope, ScopeFilter, SyncArgs, LIST_MAX_LIMIT,
    },
};

/// Owned counterpart of [`BatchVerdict`] for moving into `spawn_blocking`.
struct OwnedBatchVerdict {
    recall_id: String,
    memory_name: String,
    verdict: String,
    application_note: Option<String>,
    confidence: String,
}

/// MCP server implementation.
///
/// Each tool method is an async handler that calls into the backing subsystems
/// (git repo, embedding engine, HNSW index) and returns structured JSON.
#[derive(Clone)]
pub struct MemoryServer {
    state: Arc<AppState>,
    federation: Federation,
    // Read by the #[tool_router] macro-generated ServerHandler impl;
    // rustc's dead-code analysis can't see through proc-macro output.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

/// Extract an opaque bearer delegation from an inbound request.
///
/// The returned token excludes the `Bearer` prefix. Invalid or non-bearer
/// authorization values are treated as absent and are never logged.
fn extract_bearer(parts: &http::request::Parts) -> Option<String> {
    let value = parts
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, credential) = value.split_once(' ')?;
    let credential = credential.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !credential.is_empty()).then(|| credential.to_owned())
}

fn memory_value(memory: Memory) -> serde_json::Value {
    serde_json::json!({
        "id": memory.id,
        "name": memory.name,
        "scope": memory.metadata.scope.to_string(),
        "tags": memory.metadata.tags,
        "content": memory.content,
        "source": memory.metadata.source,
        "created_at": memory.metadata.created_at,
        "updated_at": memory.metadata.updated_at,
    })
}

/// Maximum allowed content size in bytes (1 MiB).
const MAX_CONTENT_SIZE: usize = 1_048_576;

/// Default and hard maximum number of summaries returned by `list`.
const LIST_DEFAULT_LIMIT: usize = 50;
/// Hard ceiling for the serialized JSON returned by one successful `list` page.
const LIST_PAGE_MAX_BYTES: usize = 24 * 1024;
/// Bounds work performed while decoding an untrusted opaque cursor.
const LIST_CURSOR_MAX_CHARS: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ListSortKey {
    scope: String,
    name: String,
}

impl ListSortKey {
    fn from_memory(memory: &Memory) -> Self {
        Self {
            scope: memory.metadata.scope.to_string(),
            name: memory.name.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ListCursorPayload {
    version: u8,
    filter: String,
    scope: String,
    name: String,
}

#[derive(Debug)]
struct DecodedListCursor {
    key: ListSortKey,
}

fn invalid_list_input(reason: impl Into<String>) -> MemoryError {
    MemoryError::InvalidInput {
        reason: reason.into(),
    }
}

fn validate_list_limit(limit: Option<usize>) -> Result<usize, MemoryError> {
    let limit = limit.unwrap_or(LIST_DEFAULT_LIMIT);
    if !(1..=LIST_MAX_LIMIT).contains(&limit) {
        return Err(invalid_list_input(format!(
            "list limit must be between 1 and {LIST_MAX_LIMIT}"
        )));
    }
    Ok(limit)
}

fn list_filter_key(filter: &ScopeFilter) -> Cow<'static, str> {
    match filter {
        ScopeFilter::RootOnly => Cow::Borrowed("root"),
        ScopeFilter::All => Cow::Borrowed("all"),
        ScopeFilter::Subtree(path) => Cow::Owned(format!("subtree:{}", path.as_str())),
    }
}

fn encode_list_cursor(filter: &str, key: &ListSortKey) -> Result<String, MemoryError> {
    let payload = serde_json::to_vec(&ListCursorPayload {
        version: 1,
        filter: filter.to_string(),
        scope: key.scope.clone(),
        name: key.name.clone(),
    })
    .map_err(|error| MemoryError::Internal(format!("serialize list cursor: {error}")))?;
    let encoded = format!("lc1_{}", URL_SAFE_NO_PAD.encode(payload));
    if encoded.len() > LIST_CURSOR_MAX_CHARS {
        return Err(invalid_list_input(
            "encoded list cursor exceeds the supported size",
        ));
    }
    Ok(encoded)
}

fn decode_list_cursor(
    cursor: &str,
    expected_filter: &str,
) -> Result<DecodedListCursor, MemoryError> {
    if cursor.len() > LIST_CURSOR_MAX_CHARS {
        return Err(invalid_list_input(
            "list cursor is too large; omit cursor to start a new page",
        ));
    }
    let encoded = cursor.strip_prefix("lc1_").ok_or_else(|| {
        invalid_list_input("invalid list cursor; omit cursor to start a new page")
    })?;
    if encoded.is_empty() {
        return Err(invalid_list_input(
            "invalid list cursor; omit cursor to start a new page",
        ));
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| invalid_list_input("invalid list cursor; omit cursor to start a new page"))?;
    let payload: ListCursorPayload = serde_json::from_slice(&decoded)
        .map_err(|_| invalid_list_input("invalid list cursor; omit cursor to start a new page"))?;
    if payload.version != 1 {
        return Err(invalid_list_input(
            "unsupported list cursor version; omit cursor to start a new page",
        ));
    }
    if payload.filter != expected_filter {
        return Err(invalid_list_input(
            "list cursor belongs to a different scope query; omit cursor to start a new page",
        ));
    }
    Ok(DecodedListCursor {
        key: ListSortKey {
            scope: payload.scope,
            name: payload.name,
        },
    })
}

fn list_summary(memory: &Memory, fields: &[ListField]) -> serde_json::Value {
    let mut summary = serde_json::Map::new();
    for field in fields {
        match field {
            ListField::Id => {
                summary.insert("id".to_string(), serde_json::json!(memory.id));
            }
            ListField::Name => {
                summary.insert("name".to_string(), serde_json::json!(memory.name));
            }
            ListField::Scope => {
                summary.insert(
                    "scope".to_string(),
                    serde_json::json!(memory.metadata.scope.to_string()),
                );
            }
            ListField::Tags => {
                summary.insert("tags".to_string(), serde_json::json!(memory.metadata.tags));
            }
            ListField::CreatedAt => {
                summary.insert(
                    "created_at".to_string(),
                    serde_json::json!(memory.metadata.created_at),
                );
            }
            ListField::UpdatedAt => {
                summary.insert(
                    "updated_at".to_string(),
                    serde_json::json!(memory.metadata.updated_at),
                );
            }
        }
    }
    serde_json::Value::Object(summary)
}

fn list_page_value(
    summaries: &[serde_json::Value],
    count: usize,
    limit: usize,
    next_cursor: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "memories": summaries,
        "count": count,
        "returned": summaries.len(),
        "limit": limit,
        "has_more": next_cursor.is_some(),
        "next_cursor": next_cursor,
    })
}

fn paginate_list(
    memories: Vec<Memory>,
    filter: &ScopeFilter,
    limit: usize,
    after: Option<DecodedListCursor>,
    fields: &[ListField],
) -> Result<String, MemoryError> {
    let mut memories: Vec<_> = memories
        .into_iter()
        .map(|memory| (ListSortKey::from_memory(&memory), memory))
        .collect();
    memories.sort_by(|(left, _), (right, _)| left.cmp(right));
    let count = memories.len();
    let filter_key = list_filter_key(filter);
    let start = after.as_ref().map_or(0, |cursor| {
        memories.partition_point(|(key, _)| key <= &cursor.key)
    });

    let mut summaries = Vec::with_capacity(limit.min(count.saturating_sub(start)));
    let mut accepted_page = None;
    for (key, memory) in memories.iter().skip(start).take(limit) {
        summaries.push(list_summary(memory, fields));
        let next_index = start + summaries.len();
        let next_cursor = if next_index < count {
            Some(
                encode_list_cursor(filter_key.as_ref(), key).map_err(|error| {
                    invalid_list_input(format!(
                        "cannot continue list pagination after memory '{}' in scope '{}': {error}",
                        key.name, key.scope
                    ))
                })?,
            )
        } else {
            None
        };
        let candidate = serde_json::to_string(&list_page_value(
            &summaries,
            count,
            limit,
            next_cursor.as_deref(),
        ))
        .map_err(|error| MemoryError::Internal(format!("serialize list page: {error}")))?;
        if candidate.len() > LIST_PAGE_MAX_BYTES {
            summaries.pop();
            if summaries.is_empty() {
                return Err(invalid_list_input(format!(
                    "one list summary exceeds the {LIST_PAGE_MAX_BYTES}-byte page ceiling; request fewer fields"
                )));
            }
            break;
        }
        accepted_page = Some(candidate);
    }

    let page = match accepted_page {
        Some(page) => page,
        None => serde_json::to_string(&list_page_value(&[], count, limit, None))
            .map_err(|error| MemoryError::Internal(format!("serialize list page: {error}")))?,
    };
    if page.len() > LIST_PAGE_MAX_BYTES {
        return Err(MemoryError::Internal(
            "list page exceeded its serialized byte ceiling".to_string(),
        ));
    }
    Ok(page)
}

const SERVER_PROCESSING_DURATION_META_KEY: &str = "memory-mcp/serverProcessingDurationMs";

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn finish_tool_call(
    tool_name: &str,
    start: Instant,
    result: Result<CallToolResult, ErrorData>,
) -> Result<CallToolResult, ErrorData> {
    let server_processing_duration_ms = elapsed_ms(start);
    match result {
        Ok(mut result) => {
            let mut meta = result.meta.take().unwrap_or_else(Meta::new);
            meta.insert(
                SERVER_PROCESSING_DURATION_META_KEY.to_string(),
                server_processing_duration_ms.into(),
            );
            result.meta = Some(meta);
            info!(
                tool_name,
                server_processing_duration_ms,
                outcome = "success",
                "tool call completed"
            );
            Ok(result)
        }
        Err(error) => {
            info!(
                tool_name,
                server_processing_duration_ms,
                outcome = "error",
                "tool call completed"
            );
            Err(error)
        }
    }
}

struct EditStageTiming {
    start: Instant,
    stage: &'static str,
    outcome: &'static str,
    read_ms: u64,
    embed_total_ms: u64,
    index_ms: u64,
    repo_save_ms: u64,
}

impl EditStageTiming {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            stage: "validation",
            outcome: "error",
            read_ms: 0,
            embed_total_ms: 0,
            index_ms: 0,
            repo_save_ms: 0,
        }
    }

    fn completed(&mut self) {
        self.stage = "completed";
        self.outcome = "success";
    }
}

impl Drop for EditStageTiming {
    fn drop(&mut self) {
        info!(
            outcome = self.outcome,
            stage = self.stage,
            edit_total_ms = elapsed_ms(self.start),
            read_ms = self.read_ms,
            embed_total_ms = self.embed_total_ms,
            index_ms = self.index_ms,
            repo_save_ms = self.repo_save_ms,
            "edit stage timing"
        );
    }
}

// ---------------------------------------------------------------------------
// Cancellation-shielded mutation units
// ---------------------------------------------------------------------------

/// Run a repository-write + index-mirror unit on a detached task so request
/// cancellation can never separate a completed git commit from its index
/// mirror dispatch (#310, ADR-0039).
///
/// Repository mutations run their blocking git work detached
/// (`spawn_blocking` runs to completion even when the awaiting future is
/// dropped). Without shielding, a request cancelled at that await point can
/// commit git truth while the continuation that mirrors the write into the
/// lexical index never runs — leaving the index healthy-but-stale with no
/// repair trigger, the exact divergence class the drift contract forbids.
///
/// The unit runs on its own task: dropping the returned future (client
/// disconnect, timeout, abort) only abandons the *response*. The unit itself
/// always runs to completion, either dispatching the mirror or failing
/// through the normal paths that flag the index degraded.
///
/// Supervision is cancellation-independent (#314): the detached task itself
/// owns the dirty-marking through a [`DegradeOnDrop`] guard armed before the
/// unit runs and defused only on normal completion. If the unit panics at an
/// unknown point (possibly after the git commit) or the runtime drops the
/// task, the guard's `Drop` marks the index rebuild-required — no surviving
/// requester is needed to observe the failure. The `JoinError` arm below is
/// only the *reporting* path for a requester that is still awaiting; it is
/// not what the contract relies on.
async fn shielded_mutation_unit<T, F>(
    lexical: &Arc<LexicalIndex>,
    unit: F,
) -> Result<T, MemoryError>
where
    F: std::future::Future<Output = Result<T, MemoryError>> + Send + 'static,
    T: Send + 'static,
{
    let guard_lexical = Arc::clone(lexical);
    let worker = tokio::spawn(
        async move {
            let guard = DegradeOnDrop::new(guard_lexical, "mutation unit died before completing");
            let outcome = unit.await;
            guard.defuse();
            outcome
        }
        .in_current_span(),
    );
    match worker.await {
        Ok(outcome) => outcome,
        Err(e) => Err(MemoryError::Join(format!("mutation unit task failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// Incremental reindex helper
// ---------------------------------------------------------------------------

/// Whether splitting a failed batch into individual requests can isolate the
/// failure.
///
/// Worker lifecycle and queueing failures affect every item equally. A
/// per-item fallback would only enqueue duplicate work behind the same slow or
/// unavailable worker, amplifying one failure into a timeout cascade. An
/// active inference timeout remains splittable: the batch itself may simply
/// have exceeded the deadline while smaller requests can complete in time.
fn batch_failure_can_split(error: &MemoryError) -> bool {
    !matches!(
        error,
        MemoryError::EmbeddingQueueTimeout { .. }
            | MemoryError::EmbeddingWorkerBusy
            | MemoryError::EmbeddingWorkerUnavailable { .. }
    )
}

/// Whether a singleton fallback failure means the remaining retries would
/// only queue behind the same unhealthy worker. An inference timeout is also
/// terminal here: a one-item request cannot be split any further.
fn fallback_failure_stops_retries(error: &MemoryError) -> bool {
    matches!(
        error,
        MemoryError::EmbeddingQueueTimeout { .. }
            | MemoryError::EmbeddingInferenceTimeout { .. }
            | MemoryError::EmbeddingWorkerBusy
            | MemoryError::EmbeddingWorkerUnavailable { .. }
    )
}

/// Re-embed and re-index all memories that changed between two commits.
///
/// Removals are processed first so a name that was deleted and re-added in
/// the same pull gets a fresh entry rather than a ghost. Lexical mutations
/// are accumulated and applied as one batch (a single Tantivy commit and
/// reader reload on the blocking pool), not one commit per memory.
///
/// Complete-or-degraded (#314): the changed refs arrive pre-resolved from
/// frontmatter (see `MemoryRepo::diff_changed_refs`) and their canonical
/// keys come from `MemoryRef::qualified_path` — the same function every
/// index entry was written with — so removals and upserts can never target
/// an ambiguously derived key. Any preparation gap that would reduce the
/// lexical batch (a changed memory that cannot be read back) marks the
/// index rebuild-required *before* the reduced batch commits and schedules
/// repair, instead of committing the partial mirror as healthy.
async fn incremental_reindex(
    repo: &Arc<MemoryRepo>,
    router: &crate::repo_router::RepoRouter,
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
    lexical: &Arc<LexicalIndex>,
    changes: &ResolvedChanges,
) -> ReindexStats {
    let mut stats = ReindexStats::default();
    let mut lexical_ops: Vec<LexicalOp> = Vec::new();
    let mut lexical_gap = changes.unresolved > 0;
    if lexical_gap {
        lexical.mark_rebuild_required("pulled changes contained unresolvable memory files");
    }

    // ---- 1. Removals --------------------------------------------------------
    for mref in &changes.removed {
        let canonical = mref.qualified_path();
        lexical_ops.push(LexicalOp::Remove(canonical.clone()));
        match index.remove(&mref.scope, &canonical) {
            Ok(()) => {
                stats.removed += 1;
            }
            Err(e) => {
                warn!(
                    qualified_name = %canonical,
                    error = %e,
                    "incremental_reindex: failed to remove vector; skipping"
                );
                stats.errors += 1;
            }
        }
        // If not in index, remove is a no-op — not an error.
    }

    // ---- 2. Read upserted memories from disk ---------------------------------
    // (MemoryRef, content)
    let mut to_embed: Vec<(MemoryRef, String)> = Vec::new();
    for mref in &changes.upserted {
        let qualified = mref.qualified_path();
        let memory = match repo.read_memory(&mref.name, &mref.scope).await {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    qualified_name = %qualified,
                    error = %e,
                    "incremental_reindex: failed to read changed memory; lexical \
                     batch is incomplete — marking index rebuild-required"
                );
                stats.errors += 1;
                lexical_gap = true;
                lexical
                    .mark_rebuild_required("incremental reindex could not read a changed memory");
                continue;
            }
        };
        lexical_ops.push(LexicalOp::Upsert(LexicalDoc {
            qualified_name: qualified,
            name: memory.name.as_str().to_string(),
            content: memory.content.clone(),
        }));
        to_embed.push((mref.clone(), memory.content));
    }

    // ---- 2b. Mirror all changes into the lexical index ----------------------
    // One batch: a single commit and reader reload for the whole changed
    // set, on the blocking pool. Best-effort — a failure flags the index
    // degraded (semantic-only recall) and repair rebuilds from git truth.
    // A preparation gap above already flagged the index before this partial
    // batch could commit; repair converges it either way.
    if let Err(e) = lexical.apply_async(lexical_ops).await {
        warn!(
            error = %e,
            "incremental_reindex: lexical batch update failed; keyword search \
             degraded until repair completes"
        );
        spawn_lexical_repair_for_router(router, lexical);
    } else if lexical_gap {
        spawn_lexical_repair_for_router(router, lexical);
    }

    if to_embed.is_empty() {
        return stats;
    }

    // ---- 3. Batch embed all content -----------------------------------------
    let contents: Vec<String> = to_embed.iter().map(|(_, c)| c.clone()).collect();
    let vectors = match embedding.embed(&contents).await {
        Ok(v) => v,
        Err(batch_err) if !batch_failure_can_split(&batch_err) => {
            warn!(
                error = %batch_err,
                failed_count = to_embed.len(),
                "incremental_reindex: systemic batch embed failure; not retrying per-item"
            );
            for (mref, _) in &to_embed {
                warn!(
                    qualified_name = %mref.qualified_path(),
                    "incremental_reindex: memory not embedded after systemic batch failure"
                );
            }
            stats.errors += to_embed.len();
            to_embed.clear();
            Vec::new()
        }
        Err(batch_err) => {
            warn!(error = %batch_err, "incremental_reindex: batch embed failed; falling back to per-item");
            let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(contents.len());
            let mut failed: Vec<usize> = Vec::new();
            for (i, content) in contents.iter().enumerate() {
                match embedding.embed(std::slice::from_ref(content)).await {
                    Ok(mut v) => vecs.push(v.remove(0)),
                    Err(e) if fallback_failure_stops_retries(&e) => {
                        warn!(
                            error = %e,
                            failed_count = contents.len() - i,
                            "incremental_reindex: systemic per-item embed failure; stopping fallback"
                        );
                        for (mref, _) in &to_embed[i..] {
                            warn!(
                                qualified_name = %mref.qualified_path(),
                                "incremental_reindex: memory not embedded after fallback stopped"
                            );
                        }
                        failed.extend(i..contents.len());
                        stats.errors += contents.len() - i;
                        break;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            qualified_name = %to_embed[i].0.qualified_path(),
                            "incremental_reindex: per-item embed failed; skipping"
                        );
                        failed.push(i);
                        stats.errors += 1;
                    }
                }
            }
            // Remove failed items from to_embed in reverse order to preserve indices.
            for &i in failed.iter().rev() {
                to_embed.remove(i);
            }
            vecs
        }
    };

    // ---- 4. Update index entries --------------------------------------------
    for ((mref, _), vector) in to_embed.iter().zip(vectors.iter()) {
        let qualified_name = mref.qualified_path();
        let is_update = index.find_by_name(&qualified_name).is_some();

        match index.add(&mref.scope, vector, qualified_name.clone()) {
            Ok(_) => {}
            Err(e) => {
                warn!(
                    qualified_name = %qualified_name,
                    error = %e,
                    "incremental_reindex: add failed; skipping"
                );
                stats.errors += 1;
                continue;
            }
        }

        if is_update {
            stats.updated += 1;
        } else {
            stats.added += 1;
        }
    }

    stats
}

/// Mirror `sync_all` results into the derived indexes and advance the
/// composite index SHA when — and only when — the mirror is complete.
///
/// Runs inside sync's cancellation-shielded unit, after the pulls have moved
/// git truth. Complete-or-degraded (#314): a repo whose post-pull change
/// discovery did not complete (`changes_complete == false`) or whose pulled
/// files could not all be resolved marks the lexical index rebuild-required,
/// schedules router-wide repair, and refuses to advance the stored composite
/// SHA — the next startup reindexes from git truth instead of trusting a
/// partial mirror. The SHA is also held back when the reindex recorded *any*
/// item-level error (#293 review, round 3): a mirror that skipped even one
/// memory is not verified, so partial success must not certify it.
///
/// The refusal is sticky (#293 review, round 2): once a mirror gap opens,
/// [`AppState::mark_index_mirror_incomplete`] blocks SHA advancement for the
/// rest of the process — including later syncs whose own mirrors complete,
/// since they cannot backfill an earlier sync's unmirrored range — and the
/// shutdown persistence path honours the same flag.
async fn mirror_sync_results(
    state: &Arc<AppState>,
    sync_result: &MultiSyncResult,
) -> (ReindexStats, bool) {
    let mut total_reindex = ReindexStats::default();
    let mut any_reindex = false;
    let mut mirror_incomplete = false;
    for entry in &sync_result.results {
        if !entry.changes_complete {
            mirror_incomplete = true;
            state.lexical.mark_rebuild_required(
                "post-pull change diff failed — pulled changes not mirrored",
            );
            spawn_lexical_repair_for_router(&state.router, &state.lexical);
        }
        if let Some(ref changes) = entry.resolved_changes {
            mirror_incomplete |= changes.unresolved > 0;
            let repo = state.router.repo(&entry.scope);
            let stats = incremental_reindex(
                repo,
                &state.router,
                state.embedding.as_ref(),
                state.index.as_ref(),
                &state.lexical,
                changes,
            )
            .instrument(tracing::info_span!(
                "server.incremental_reindex",
                repo = %entry.label,
            ))
            .await;
            info!(
                repo = %entry.label,
                added = stats.added,
                updated = stats.updated,
                removed = stats.removed,
                errors = stats.errors,
                "incremental reindex complete"
            );
            total_reindex.added += stats.added;
            total_reindex.updated += stats.updated;
            total_reindex.removed += stats.removed;
            total_reindex.errors += stats.errors;
            any_reindex = true;
        }
    }

    // Any item-level error means the mirror skipped something (#293 review,
    // round 3): certification requires errors == 0, not merely "some items
    // succeeded".
    let reindex_had_errors = total_reindex.errors > 0;
    if mirror_incomplete || reindex_had_errors {
        // Sticky for the process lifetime: a later complete sync only
        // mirrors its own pulled range and cannot backfill this gap, so it
        // must not advance the SHA either — only a full reindex (next
        // startup) closes the gap and re-earns advancement.
        state.mark_index_mirror_incomplete();
    }
    if state.index_mirror_is_intact() {
        if let Some(sha) = state.router.head_sha().await {
            state.index.set_commit_sha(Some(&sha));
        }
    }

    (total_reindex, any_reindex)
}

/// Per-repo failure detail carried in the `sync` tool's error payload.
#[derive(Debug, Serialize)]
struct SyncRepoFailure {
    /// Repo label (`"default"` or the scope prefix).
    label: String,
    /// Pull error message, when the pull failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_error: Option<String>,
    /// Push error message, when the push failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    push_error: Option<String>,
}

/// Build the `sync` tool error for a sync where at least one repo's pull or
/// push failed.
///
/// Matches the single-repo contract where pull/push failures surface as MCP
/// tool errors: the message enumerates every failed repo with its error, and
/// the structured `data` payload carries the failed entries plus the labels
/// of repos that synced cleanly, so partial success stays visible.
fn sync_failure_error(sync_result: &MultiSyncResult, branch: &str) -> ErrorData {
    let failed: Vec<SyncRepoFailure> = sync_result
        .results
        .iter()
        .filter(|e| !e.is_ok())
        .map(|e| SyncRepoFailure {
            label: e.label.clone(),
            pull_error: e.pull_error.clone(),
            push_error: e.push_error.clone(),
        })
        .collect();
    let succeeded: Vec<&str> = sync_result
        .results
        .iter()
        .filter(|e| e.is_ok())
        .map(|e| e.label.as_str())
        .collect();
    let summary = sync_result
        .results
        .iter()
        .filter_map(|e| e.failure_summary().map(|s| format!("{}: {s}", e.label)))
        .collect::<Vec<_>>()
        .join("; ");
    ErrorData {
        code: rmcp::model::ErrorCode::INTERNAL_ERROR,
        message: Cow::Owned(format!(
            "sync failed for {}/{} repos: {summary}",
            failed.len(),
            sync_result.results.len()
        )),
        data: Some(serde_json::json!({
            "status": "sync failed",
            "branch": branch,
            "failed": failed,
            "succeeded": succeeded,
        })),
    }
}

/// Mirror a pull's git changes into the vector and lexical indexes.
///
/// Runs inside sync's cancellation-shielded unit, after `repo.pull` has
/// already moved git truth. Complete-or-degraded (#314): any failure
/// preparing the mirror — the pulled-range diff erroring, the diff task
/// dying, or pulled files that cannot be resolved to memory references —
/// marks the lexical index rebuild-required and schedules repair, so a
/// partial or stale mirror can never keep reporting `Available`.
#[cfg(test)]
async fn mirror_pulled_changes(
    state: &Arc<AppState>,
    old_head: [u8; 20],
    new_head: [u8; 20],
) -> Result<Option<ReindexStats>, MemoryError> {
    let repo = Arc::clone(&state.repo);
    let changes = match crate::repo::traced_spawn_blocking(move || {
        repo.diff_changed_refs(old_head, new_head)
    })
    .await
    {
        Ok(Ok(changes)) => changes,
        Ok(Err(e)) => {
            state.lexical.mark_rebuild_required(
                "post-pull change diff failed — pulled changes not mirrored",
            );
            spawn_lexical_repair_for_router(&state.router, &state.lexical);
            return Err(e);
        }
        Err(e) => {
            state
                .lexical
                .mark_rebuild_required("post-pull change diff task did not run to completion");
            spawn_lexical_repair_for_router(&state.router, &state.lexical);
            return Err(MemoryError::Join(e.to_string()));
        }
    };

    if changes.unresolved > 0 {
        // Some pulled files could not be resolved to memory references, so
        // the mirror below is incomplete by construction. Flag *before* the
        // reduced batch commits; repair (a rebuild from `list_memories`
        // truth, which applies the same resolution rules) converges
        // deterministically.
        warn!(
            unresolved = changes.unresolved,
            "pulled changes contained unresolvable memory files; lexical \
             mirror incomplete — marking index rebuild-required"
        );
        state
            .lexical
            .mark_rebuild_required("pulled changes contained unresolvable memory files");
    }

    let mut reindex_stats = None;
    let mut reindex_had_errors = false;
    if !changes.is_empty() {
        let stats = incremental_reindex(
            &state.repo,
            &state.router,
            state.embedding.as_ref(),
            state.index.as_ref(),
            &state.lexical,
            &changes,
        )
        .instrument(tracing::info_span!("server.incremental_reindex"))
        .await;
        info!(
            added = stats.added,
            updated = stats.updated,
            removed = stats.removed,
            errors = stats.errors,
            "incremental reindex complete"
        );
        reindex_had_errors = stats.errors > 0;
        reindex_stats = Some(stats);
    }

    if changes.unresolved > 0 {
        spawn_lexical_repair_for_router(&state.router, &state.lexical);
    }

    // Advance the stored SHA so the next startup doesn't trigger a full
    // reindex for changes already processed. Skip when any item-level error
    // occurred or the change set was incomplete, so the next startup
    // retries (#293 review, round 3).
    if !reindex_had_errors && changes.unresolved == 0 {
        if let Some(sha) = state.repo.head_sha().await {
            state.index.set_commit_sha(Some(&sha));
        }
    }

    Ok(reindex_stats)
}

/// Re-embed and re-index all memories across all repos.
///
/// This is a full rebuild: all memories are listed (via the router, which
/// aggregates across scope-specific repos), their content is embedded, and the
/// index is updated. Intended for startup freshness checks and recovery after
/// a crash that discarded an in-progress index.
///
/// Unlike delegating to `incremental_reindex`, this function uses the content
/// already loaded by `list_memories` to avoid reading each file a second time.
pub async fn full_reindex(
    router: &crate::repo_router::RepoRouter,
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
) -> Result<ReindexStats, MemoryError> {
    let memories = router.list_memories_strict().await?;
    if memories.is_empty() {
        return Ok(ReindexStats::default());
    }

    let mut stats = ReindexStats::default();

    let items: Vec<(MemoryRef, String)> = memories
        .into_iter()
        .map(|m| {
            let mref = MemoryRef::new(m.metadata.scope, m.name);
            (mref, m.content)
        })
        .collect();

    // Embed and index in chunks so each embed() call maps to roughly one
    // BERT forward pass (MAX_BATCH_SIZE=64 inside the worker) and stays
    // within the per-call timeout budget.
    const REINDEX_BATCH_SIZE: usize = 64;
    for (chunk_index, chunk) in items.chunks(REINDEX_BATCH_SIZE).enumerate() {
        let chunk_start = chunk_index * REINDEX_BATCH_SIZE;
        let remaining_after_chunk = items.len() - (chunk_start + chunk.len());
        let mut stop_after_chunk = false;
        let contents: Vec<String> = chunk.iter().map(|(_, c)| c.clone()).collect();

        let vectors = match embedding.embed(&contents).await {
            Ok(v) => v,
            Err(batch_err) if !batch_failure_can_split(&batch_err) => {
                warn!(
                    error = %batch_err,
                    failed_count = items.len() - chunk_start,
                    "full_reindex: systemic batch embed failure; not retrying per-item"
                );
                for (mref, _) in &items[chunk_start..] {
                    warn!(
                        qualified_name = %mref.qualified_path(),
                        "full_reindex: memory not embedded after systemic batch failure"
                    );
                }
                stats.errors += items.len() - chunk_start;
                stop_after_chunk = true;
                vec![Vec::new(); chunk.len()]
            }
            Err(batch_err) => {
                warn!(error = %batch_err, "full_reindex: batch embed failed; falling back to per-item");
                let mut vecs = Vec::with_capacity(contents.len());
                for (i, content) in contents.iter().enumerate() {
                    match embedding.embed(std::slice::from_ref(content)).await {
                        Ok(mut v) => vecs.push(v.remove(0)),
                        Err(e) if fallback_failure_stops_retries(&e) => {
                            warn!(
                                error = %e,
                                failed_count = contents.len() - i,
                                "full_reindex: systemic per-item embed failure; stopping fallback"
                            );
                            for (mref, _) in &chunk[i..] {
                                warn!(
                                    qualified_name = %mref.qualified_path(),
                                    "full_reindex: memory not embedded after fallback stopped"
                                );
                            }
                            stats.errors += contents.len() - i;
                            for (mref, _) in &items[chunk_start + chunk.len()..] {
                                warn!(
                                    qualified_name = %mref.qualified_path(),
                                    "full_reindex: memory not embedded after fallback stopped"
                                );
                            }
                            stats.errors += remaining_after_chunk;
                            stop_after_chunk = true;
                            vecs.extend((i..contents.len()).map(|_| Vec::new()));
                            break;
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                qualified_name = %chunk[i].0.qualified_path(),
                                "full_reindex: per-item embed failed; skipping"
                            );
                            stats.errors += 1;
                            vecs.push(Vec::new());
                        }
                    }
                }
                vecs
            }
        };

        debug_assert_eq!(
            vectors.len(),
            chunk.len(),
            "embed() must return exactly one vector per input"
        );

        for ((mref, _), vector) in chunk.iter().zip(vectors.iter()) {
            if vector.is_empty() {
                continue;
            }
            let qualified_name = mref.qualified_path();
            match index.add(&mref.scope, vector, qualified_name.clone()) {
                Ok(_) => stats.added += 1,
                Err(e) => {
                    warn!(
                        qualified_name = %qualified_name,
                        error = %e,
                        "full_reindex: index add failed; skipping"
                    );
                    stats.errors += 1;
                }
            }
        }

        if stop_after_chunk {
            break;
        }
    }

    Ok(stats)
}

/// Run the startup full reindex and certify the result.
///
/// Stamps `head_sha` into the index only when the rebuild recorded zero
/// item-level errors (#293 review, round 3): a rebuild that skipped even one
/// memory is not a verified mirror, so the stamp is withheld — the next
/// startup sees the SHA mismatch and rebuilds again — instead of certifying
/// an incomplete index as intact.
///
/// Returns `true` when the reindex completed cleanly and was certified.
pub async fn startup_reindex_and_certify(
    router: &crate::repo_router::RepoRouter,
    embedding: &dyn EmbeddingBackend,
    index: &dyn VectorStore,
    head_sha: Option<&str>,
) -> bool {
    match full_reindex(router, embedding, index).await {
        Ok(stats) => {
            info!(
                added = stats.added,
                errors = stats.errors,
                "startup reindex complete"
            );
            if stats.errors == 0 {
                if let Some(sha) = head_sha {
                    index.set_commit_sha(Some(sha));
                }
                true
            } else {
                warn!(
                    added = stats.added,
                    errors = stats.errors,
                    "startup reindex recorded item-level errors — SHA not stamped, \
                     the next startup rebuilds instead of trusting a partial index"
                );
                false
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "startup reindex failed — SHA not stamped, will retry on next startup"
            );
            false
        }
    }
}

/// Prepare the vector index to serve from at startup: run the initial pull
/// (when requested) **before** deciding index freshness, then rebuild and
/// certify against post-pull git truth when the stored SHA is stale.
///
/// The ordering is the contract (#327): on a fresh repo path with a
/// configured remote, the initial pull is what populates the repo. Deciding
/// freshness — or rebuilding — before the pull completes certifies an index
/// of the pre-pull (typically empty) tree, and the server signals ready
/// while semantic recall misses every pulled memory.
///
/// The pull covers **every** routed repo — the default plus each configured
/// scope-mapped remote, respecting per-route branch overrides (#328 review,
/// round 2). Pulling only the default repo would leave mapped repos at their
/// local init commits: the composite-HEAD reindex would certify their empty
/// trees and readiness could go green while every remotely stored
/// mapped-scope memory is absent until an explicit sync.
///
/// `initial_pull` carries `(auth, default_branch)` when
/// `--require-remote-sync` is set; a pull failure on any repo is logged and
/// startup continues (aggregate sync health shows degraded), matching the
/// previous behavior.
///
/// Returns the index to serve from plus `reindex_ok`: `false` when a rebuild
/// ran and did not certify cleanly, in which case the caller must record a
/// startup mirror gap so SHA advancement stays blocked for this process.
pub async fn startup_prepare_index(
    initial_pull: Option<(&crate::auth::AuthProvider, &str)>,
    router: &crate::repo_router::RepoRouter,
    embedding: &dyn EmbeddingBackend,
    loaded_index: Box<dyn VectorStore>,
    fresh_index: impl FnOnce() -> Box<dyn VectorStore>,
) -> (Box<dyn VectorStore>, bool) {
    if let Some((auth, default_branch)) = initial_pull {
        info!("--require-remote-sync: performing initial pull across all repos");
        if !router.pull_all(auth, default_branch).await {
            warn!("initial pull failed for one or more repos — sync health will show degraded");
        }
    }

    // Freshness is judged against git truth as it stands *after* the pull.
    // If the stored SHA doesn't match, discard the loaded index entirely and
    // rebuild from scratch — this prevents ghost entries from deleted
    // memories lingering.
    let head_sha = router.head_sha().await;
    if head_sha == loaded_index.commit_sha() {
        tracing::debug!(sha = ?head_sha, "index SHA matches repo HEAD — skipping reindex");
        return (loaded_index, true);
    }

    info!(
        head = ?head_sha,
        index = ?loaded_index.commit_sha(),
        "index SHA does not match repo HEAD — rebuilding from scratch"
    );
    let index = fresh_index();

    // Certification requires errors == 0 (#293 review, round 3): a partial
    // rebuild must not stamp the SHA, or the next startup would see index
    // SHA == HEAD and skip the reindex that repairs the gap.
    let reindex_ok =
        startup_reindex_and_certify(router, embedding, index.as_ref(), head_sha.as_deref())
            .instrument(tracing::info_span!("startup.full_reindex"))
            .await;
    (index, reindex_ok)
}

/// Persist the vector index at graceful shutdown.
///
/// Advances the stored composite SHA to the router's current HEAD only when
/// the in-process mirror is intact ([`AppState::index_mirror_is_intact`]).
/// When a mirror gap was recorded — e.g. a sync's post-pull change discovery
/// failed and the sync mirror refused SHA advancement — the index
/// keeps the SHA it last legitimately verified, so the next startup sees the
/// mismatch against git HEAD and rebuilds the vector index from git truth
/// instead of trusting the incomplete mirror (#293 review, round 2).
pub async fn persist_index_on_shutdown(
    state: &AppState,
    index_dir: &std::path::Path,
) -> Result<(), MemoryError> {
    std::fs::create_dir_all(index_dir)?;
    if state.index_mirror_is_intact() {
        if let Some(sha) = state.router.head_sha().await {
            state.index.set_commit_sha(Some(&sha));
        }
    } else {
        info!(
            stored_sha = ?state.index.commit_sha(),
            "index mirror incomplete — keeping last verified SHA so the next startup reindexes"
        );
    }
    state.index.save(index_dir)
}

#[tool_router]
impl MemoryServer {
    /// Create a new MCP server backed by the given application state.
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            federation: Federation::default(),
            tool_router: Self::tool_router(),
        }
    }

    /// Create a server with validated sibling-store topology.
    pub fn with_config(state: Arc<AppState>, config: &Config) -> Result<Self, MemoryError> {
        Ok(Self {
            state,
            federation: Federation::from_config(config)?,
            tool_router: Self::tool_router(),
        })
    }

    /// Store a new memory in the git-backed repository.
    ///
    /// Writes `<scope>/<name>.md` with YAML frontmatter, commits to git,
    /// and indexes the content for semantic retrieval.
    ///
    /// Returns the assigned memory ID on success.
    #[tool(
        name = "remember",
        description = "Store (write) a new memory. Prefer the canonical `note` tool — `remember` \
        is the same operation under its original name. Saves the content to the git-backed \
        repository and indexes it for semantic search. Use scope '<basename-of-your-cwd>' or \
        'org/team' for scoped memories, or omit for global. Returns the assigned memory ID. \
        IMPORTANT: Never store credentials, API keys, tokens, passwords, or other secrets — \
        memories are plaintext files in a git repo and may be synced to a remote."
    )]
    async fn remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let name = MemoryName::new(args.name).map_err(ErrorData::from)?;
        if args.content.len() > MAX_CONTENT_SIZE {
            return Err(ErrorData::from(crate::error::MemoryError::InvalidInput {
                reason: format!(
                    "content size {} exceeds maximum of {} bytes",
                    args.content.len(),
                    MAX_CONTENT_SIZE
                ),
            }));
        }
        let session_id = extract_session_id(&parts);
        let content_size = args.content.len();
        let span = tracing::info_span!(
            "handler.remember",
            session_id = %session_id,
            name = %name,
            scope = ?args.scope,
            content_size,
        );
        let state = Arc::clone(&self.state);
        async move {
            let scope = Scope::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;
            let metadata = MemoryMetadata::new(scope.clone(), args.tags, args.source);
            let memory = Memory::from_validated(name, args.content, metadata);

            // Order: (1) embed, (2) add to index, (3) save to repo.
            // If step 3 fails, index has a stale entry (harmless — recall will skip it).
            // If step 1 or 2 fail, no repo commit happens.
            let start = Instant::now();
            let vector = state
                .embedding
                .embed_one(&memory.content)
                .await
                .map_err(ErrorData::from)?;
            info!(embed_ms = start.elapsed().as_millis(), "embedded");

            let qualified_name = memory.mem_ref().qualified_path();

            state
                .index
                .add(&scope, &vector, qualified_name.clone())
                .map_err(ErrorData::from)?;

            // Repo save + lexical mirror run as one cancellation-shielded
            // unit: cancelling this request can no longer commit the save to
            // git while stranding the mirror dispatch (#310, ADR-0039).
            let start = Instant::now();
            let unit_state = Arc::clone(&state);
            let memory = shielded_mutation_unit(&state.lexical, async move {
                unit_state.router.save_memory(&memory).await?;
                info!(repo_ms = start.elapsed().as_millis(), "saved to repo");

                // Mirror into the lexical index after git truth is durable,
                // so a save failure never leaves the index ahead of the repo.
                // The batch is dispatched eagerly (no await point between the
                // save and the dispatch). Best-effort: a failure flags the
                // index degraded and repair rebuilds it from git truth.
                if let Err(e) = unit_state
                    .lexical
                    .apply_async(vec![LexicalOp::Upsert(LexicalDoc {
                        qualified_name,
                        name: memory.name.as_str().to_string(),
                        content: memory.content.clone(),
                    })])
                    .await
                {
                    warn!(
                        name = %memory.name,
                        error = %e,
                        "lexical index update failed during remember; keyword search \
                         degraded until repair completes"
                    );
                    spawn_lexical_repair_for_router(&unit_state.router, &unit_state.lexical);
                }
                Ok(memory)
            })
            .await
            .map_err(ErrorData::from)?;

            Ok(serde_json::json!({
                "id": memory.id,
                "name": memory.name,
                "scope": memory.metadata.scope.to_string(),
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// Write a new memory. Canonical entry point for memory writes; routed to
    /// the same handler as [`remember`](Self::remember).
    #[tool(
        name = "note",
        description = "Write a new memory. Saves the content to the git-backed repository and \
        indexes it for semantic search. Use scope '<basename-of-your-cwd>' or 'org/team' for \
        scoped memories, or omit for global. Returns the assigned memory ID. \
        IMPORTANT: Never store credentials, API keys, tokens, passwords, or other secrets — \
        memories are plaintext files in a git repo and may be synced to a remote."
    )]
    async fn note(
        &self,
        args: Parameters<RememberArgs>,
        parts: Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        self.remember(args, parts).await
    }

    /// Search memories by semantic similarity to a natural-language query.
    ///
    /// Embeds the query, searches the HNSW index, and returns the top-k
    /// most relevant memories with their names, scopes, and content snippets.
    ///
    /// Returns a JSON array of matching memories sorted by relevance.
    #[tool(
        name = "recall",
        description = "Search memories with hybrid retrieval: semantic similarity (embeddings) and keyword/BM25 \
        search run in parallel and are rank-fused, so exact phrases buried in long memories still surface. \
        Returns the top matching memories as a JSON array with name, scope, tags, and a content snippet (max 500 chars).\n\n\
        Each result includes `truncated` (bool) and `content_length` (total character count). \
        When `truncated` is true, the snippet is incomplete — use the `read` tool with the memory's name and scope \
        to retrieve the full content before acting on it.\n\n\
        limit defaults to 5 and values above 100 are clamped to 100. \
        Each result also includes `match_type` ('semantic', 'lexical', or 'both') and `distance` \
        (cosine distance, lower is more similar; always numeric — lexical-only hits, which have no \
        embedding distance, carry the sentinel -1.0).\n\n\
        Scope: pass '<basename-of-your-cwd>' or 'org/team' to search that scope + global memories, \
        'global' for global-only, or 'all' to search everything. Omitting scope defaults to global-only."
    )]
    async fn recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let session_id = extract_session_id(&parts);
        let recall_id = RecallLog::generate_recall_id();
        // Note: query text is intentionally omitted from the span (R-17 privacy decision).
        let span = tracing::info_span!(
            "handler.recall",
            session_id = %session_id,
            recall_id = %recall_id,
            scope = ?args.scope,
            limit = ?args.limit,
            count = tracing::field::Empty,
        );
        let state = Arc::clone(&self.state);
        async move {
            // Parse optional scope filter.
            let scope_filter =
                ScopeFilter::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;

            let limit = args.limit.unwrap_or(5).min(100);

            // Degraded lexical index self-heals on read: kick a background
            // rebuild from git truth (single-flight). This query — and every
            // query until the rebuild converges — serves semantic-only.
            if state.lexical.is_degraded() {
                spawn_lexical_repair_for_router(&state.router, &state.lexical);
            }

            // Semantic and lexical retrieval run in parallel; ranked lists
            // are merged with reciprocal rank fusion so an exact keyword hit
            // can surface even when its embedding distance is poor.
            let start = Instant::now();
            let fused = hybrid_search(
                state.embedding.as_ref(),
                state.index.as_ref(),
                &state.lexical,
                &scope_filter,
                &args.query,
                limit,
            )
            .await
            .map_err(ErrorData::from)?;
            info!(
                search_ms = start.elapsed().as_millis(),
                candidates = fused.len(),
                "hybrid search complete"
            );

            let pre_filter_count = fused.len();
            let mut results_vec = Vec::new();
            let mut log_entries: Vec<RecallResult> = Vec::new();
            let mut skipped_errors: usize = 0;

            for hit in fused {
                // Fusion returns at most `limit` candidates; this guard is a safety
                // net that only activates if more candidates arrive than expected.
                if results_vec.len() >= limit {
                    break;
                }
                let mref = match parse_qualified_name(&hit.qualified_name) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(
                            qualified_name = %hit.qualified_name,
                            error = %e,
                            "could not parse qualified name from index; skipping"
                        );
                        skipped_errors += 1;
                        continue;
                    }
                };

                // Read the memory; if it was deleted but still in the index, skip it.
                let memory = match state.router.read_memory(&mref.name, &mref.scope).await {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(
                            name = %mref.name,
                            error = %e,
                            "could not read memory from repo (deleted?); skipping"
                        );
                        skipped_errors += 1;
                        continue;
                    }
                };

                let rank = results_vec.len();

                log_entries.push(RecallResult {
                    memory_name: memory.name.to_string(),
                    scope: memory.metadata.scope.to_string(),
                    rank,
                    // Lexical-only hits have no embedding distance; the -1.0
                    // sentinel keeps them out of distance-bucketed recall
                    // stats (which filter on distance >= 0.0).
                    distance: hit
                        .semantic_distance
                        .map_or(LEXICAL_ONLY_DISTANCE_SENTINEL, f64::from),
                });

                results_vec.push(recall_entry_json(&memory, &hit));
            }

            if let Some(ref log) = state.recall_log {
                if let Err(e) = log.log_results(&recall_id, &session_id, &log_entries) {
                    warn!(error = %e, "failed to write recall log entries");
                }
            }

            let count = results_vec.len();
            tracing::Span::current().record("count", count);
            info!(returned = count, skipped_errors, "recall complete");

            Ok(serde_json::json!({
                "recall_id": recall_id,
                "results": results_vec,
                "count": count,
                "limit": limit,
                "pre_filter_count": pre_filter_count,
                "skipped_errors": skipped_errors,
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// Delete a memory from the repository and vector index.
    ///
    /// Removes the file from the git working tree, commits the deletion,
    /// and removes the corresponding vector from the HNSW index.
    ///
    /// Returns `"ok"` on success.
    #[tool(
        name = "forget",
        description = "Delete a memory by name. Use a bare path scope like '<basename-of-your-cwd>' for scoped \
        memories or omit for global. Removes the file from git and the vector from the search index. \
        Returns 'ok' on success."
    )]
    async fn forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let name = MemoryName::new(args.name).map_err(ErrorData::from)?;
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.forget",
            session_id = %session_id,
            name = %name,
            scope = ?args.scope,
        );
        let state = Arc::clone(&self.state);
        async move {
            let scope = Scope::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;

            let start = Instant::now();

            // Repo delete + index mirrors run as one cancellation-shielded
            // unit: cancelling this request can no longer commit the git
            // deletion while stranding the index removals (#310, ADR-0039).
            let unit_state = Arc::clone(&state);
            let unit_name = name.clone();
            shielded_mutation_unit(&state.lexical, async move {
                // Delete from repo first — if this fails, index is untouched,
                // memory stays functional.
                unit_state
                    .router
                    .delete_memory(&unit_name, &scope)
                    .await?;

                // Remove from index (best-effort — stale entries are skipped at recall time).
                let qualified_name =
                    MemoryRef::new(scope.clone(), unit_name.clone()).qualified_path();
                if let Err(e) = unit_state.index.remove(&scope, &qualified_name) {
                    warn!(name = %unit_name, error = %e, "vector removal failed during forget; stale entry will be skipped at recall");
                }
                if let Err(e) = unit_state
                    .lexical
                    .apply_async(vec![LexicalOp::Remove(qualified_name)])
                    .await
                {
                    warn!(name = %unit_name, error = %e, "lexical removal failed during forget; keyword search degraded until repair completes");
                    spawn_lexical_repair_for_router(&unit_state.router, &unit_state.lexical);
                }
                Ok(())
            })
            .await
            .map_err(ErrorData::from)?;

            info!(
                ms = start.elapsed().as_millis(),
                name = %name,
                "memory forgotten"
            );

            Ok("ok".to_string())
        }
        .instrument(span)
        .await
    }

    /// Update the content or tags of an existing memory.
    ///
    /// Supports partial updates: omit `content` to keep the existing body,
    /// omit `tags` to keep the existing tags. The `updated_at` timestamp is
    /// refreshed, the change is committed to git, and the vector index is
    /// updated with a fresh embedding.
    ///
    /// Returns the updated memory ID.
    #[tool(
        name = "edit",
        description = "Edit an existing memory. Supports partial updates — omit content or \
        tags to preserve existing values. Re-embeds and re-indexes the memory. Use a bare path \
        scope like '<basename-of-your-cwd>' for scoped memories. Returns the memory ID. \
        IMPORTANT: Never store credentials, API keys, tokens, passwords, or other secrets — \
        memories are plaintext files in a git repo and may be synced to a remote."
    )]
    async fn edit(
        &self,
        Parameters(args): Parameters<EditArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let mut timing = EditStageTiming::new();
        let name = MemoryName::new(args.name).map_err(ErrorData::from)?;
        if args.content.is_none() && args.tags.is_none() {
            return Err(ErrorData::from(crate::error::MemoryError::InvalidInput {
                reason: "nothing to update — provide content or tags".into(),
            }));
        }
        if let Some(ref content) = args.content {
            if content.len() > MAX_CONTENT_SIZE {
                return Err(ErrorData::from(crate::error::MemoryError::InvalidInput {
                    reason: format!(
                        "content size {} exceeds maximum of {} bytes",
                        content.len(),
                        MAX_CONTENT_SIZE
                    ),
                }));
            }
        }
        let session_id = extract_session_id(&parts);
        let content_size = args.content.as_ref().map(|c| c.len()).unwrap_or(0);
        let span = tracing::info_span!(
            "handler.edit",
            session_id = %session_id,
            name = %name,
            scope = ?args.scope,
            content_size,
        );
        let state = Arc::clone(&self.state);
        async move {
            let scope = Scope::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;

            // Track whether content changed so we can skip re-embedding when only tags changed.
            let content_changed = args.content.is_some();

            // Read the existing memory.
            timing.stage = "read";
            let stage_start = Instant::now();
            let read_result = state.router.read_memory(&name, &scope).await;
            timing.read_ms = elapsed_ms(stage_start);
            let mut memory = read_result.map_err(ErrorData::from)?;

            // Apply partial updates.
            if let Some(content) = args.content {
                memory.content = content;
            }
            if let Some(tags) = args.tags {
                memory.metadata.tags = tags;
            }
            memory.metadata.updated_at = Utc::now();

            // Only re-embed and re-index when content changed.
            // Order: (1) embed, (2) upsert index entry, (3) save to repo.
            if content_changed {
                let qualified_name = memory.mem_ref().qualified_path();

                // Re-embed updated content.
                timing.stage = "embed";
                let stage_start = Instant::now();
                let embed_result = state.embedding.embed_one(&memory.content).await;
                timing.embed_total_ms = elapsed_ms(stage_start);
                let vector = embed_result.map_err(ErrorData::from)?;

                timing.stage = "index";
                let stage_start = Instant::now();
                let index_result = state.index.add(&scope, &vector, qualified_name.clone());
                timing.index_ms = elapsed_ms(stage_start);
                index_result.map_err(ErrorData::from)?;
            }

            // Persist to repo (last, so partial failures leave recoverable
            // state). Save + lexical mirror run as one cancellation-shielded
            // unit: cancelling this request can no longer commit the save to
            // git while stranding the mirror dispatch (#310, ADR-0039).
            timing.stage = "repo_save";
            let unit_state = Arc::clone(&state);
            let (memory, mut timing) = shielded_mutation_unit(&state.lexical, async move {
                let stage_start = Instant::now();
                let save_result = unit_state.router.save_memory(&memory).await;
                timing.repo_save_ms = elapsed_ms(stage_start);
                save_result?;

                // Mirror into the lexical index after git truth is durable,
                // so a save failure never leaves the index serving content
                // the repo does not hold. Eager dispatch: no await point
                // between the save and the dispatch. Best-effort: a failure
                // flags the index degraded and repair rebuilds it from git
                // truth.
                if content_changed {
                    if let Err(e) = unit_state
                        .lexical
                        .apply_async(vec![LexicalOp::Upsert(LexicalDoc {
                            qualified_name: memory.mem_ref().qualified_path(),
                            name: memory.name.as_str().to_string(),
                            content: memory.content.clone(),
                        })])
                        .await
                    {
                        warn!(
                            name = %memory.name,
                            error = %e,
                            "lexical index update failed during edit; keyword search \
                             degraded until repair completes"
                        );
                        spawn_lexical_repair_for_router(&unit_state.router, &unit_state.lexical);
                    }
                }
                Ok((memory, timing))
            })
            .await
            .map_err(ErrorData::from)?;

            info!(
                ms = elapsed_ms(timing.start),
                name = %name,
                content_changed,
                "memory edited"
            );

            timing.completed();

            Ok(serde_json::json!({
                "id": memory.id,
                "name": memory.name,
                "scope": memory.metadata.scope.to_string(),
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// Move a memory from one scope to another, optionally renaming it.
    ///
    /// Reads the source memory, creates it in the destination scope (with a
    /// new name if provided), re-indexes the embedding, then deletes the
    /// source. Atomic: if the create fails, the source is preserved.
    ///
    /// Returns the new memory ID, name, and scope on success.
    #[tool(
        name = "move",
        description = "Move a memory from one scope to another, optionally renaming it. \
        Reads the source memory (content, tags, metadata), creates it in the destination scope, \
        re-indexes the embedding, and deletes the original. Atomic: if the create fails, the \
        source is preserved. Use 'new_name' to rename during the move. Returns the new memory \
        ID, name, and scope."
    )]
    async fn move_memory(
        &self,
        Parameters(args): Parameters<MoveArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let name = MemoryName::new(args.name).map_err(ErrorData::from)?;
        let new_name = match args.new_name {
            Some(n) => MemoryName::new(n).map_err(ErrorData::from)?,
            None => name.clone(),
        };
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.move",
            session_id = %session_id,
            name = %name,
            new_name = %new_name,
            from_scope = ?args.from_scope,
            to_scope = %args.to_scope,
        );
        let state = Arc::clone(&self.state);
        async move {
            let from_scope =
                Scope::parse_or_default(args.from_scope.as_deref()).map_err(ErrorData::from)?;
            let to_scope =
                Scope::parse_or_default(Some(&args.to_scope)).map_err(ErrorData::from)?;

            // Reject no-op moves (same scope + same name).
            if from_scope == to_scope && name == new_name {
                return Err(ErrorData::from(crate::error::MemoryError::InvalidInput {
                    reason: "source and destination are identical — nothing to move".into(),
                }));
            }

            let start = Instant::now();

            // 1. Reject early if the destination already exists.
            match state.router.read_memory(&new_name, &to_scope).await {
                Ok(_) => {
                    return Err(ErrorData::from(crate::error::MemoryError::InvalidInput {
                        reason: format!(
                            "destination memory '{}' already exists in scope '{}' — \
                             rename or delete it first",
                            new_name, to_scope
                        ),
                    }));
                }
                Err(crate::error::MemoryError::NotFound { .. }) => {
                    // Good — destination is clear.
                }
                Err(e) => {
                    return Err(ErrorData::from(e));
                }
            }

            // Steps 2-6 run as one cancellation-shielded unit: cancelling
            // this request can no longer commit the git move while stranding
            // the index mirrors (#310, ADR-0039). The lexical mirror is
            // dispatched directly after the git commit — before the embedding
            // await — so neither cancellation nor an embedding failure can
            // leave a committed move lexically unmirrored.
            let unit_state = Arc::clone(&state);
            let unit_name = name.clone();
            let unit_new_name = new_name.clone();
            let unit_from_scope = from_scope.clone();
            let unit_to_scope = to_scope.clone();
            let dest = shielded_mutation_unit(&state.lexical, async move {
                // 2. Atomically read source, write destination, delete source
                //    in one git commit. Must happen before index mutations so a
                //    failure leaves the index consistent with the repo on disk.
                let dest = match unit_state
                    .router
                    .move_memory(&unit_name, &unit_from_scope, &unit_new_name, &unit_to_scope)
                    .await
                {
                    Ok(dest) => dest,
                    Err(e) => {
                        // Cross-repo moves can fail after the destination was
                        // committed but before the source was deleted. The
                        // exact git outcome is then uncertain, so rebuild the
                        // lexical index from all repository truth.
                        unit_state
                            .lexical
                            .mark_rebuild_required("repo move failed with an uncertain git outcome");
                        spawn_lexical_repair_for_router(
                            &unit_state.router,
                            &unit_state.lexical,
                        );
                        return Err(e);
                    }
                };

                let dest_qualified = dest.mem_ref().qualified_path();
                let source_qualified =
                    MemoryRef::new(unit_from_scope.clone(), unit_name.clone()).qualified_path();

                // 3. Mirror the move in the lexical index (best-effort). One
                //    batch: destination upsert + source removal share a single
                //    commit and reader reload on the blocking pool.
                if let Err(e) = unit_state
                    .lexical
                    .apply_async(vec![
                        LexicalOp::Upsert(LexicalDoc {
                            qualified_name: dest_qualified.clone(),
                            name: dest.name.as_str().to_string(),
                            content: dest.content.clone(),
                        }),
                        LexicalOp::Remove(source_qualified.clone()),
                    ])
                    .await
                {
                    warn!(
                        name = %unit_name,
                        new_name = %unit_new_name,
                        error = %e,
                        "lexical index update failed during move; keyword search degraded \
                         until repair completes"
                    );
                    spawn_lexical_repair_for_router(&unit_state.router, &unit_state.lexical);
                }

                // 4. Embed the content for the new scope's index entry.
                let vector = unit_state.embedding.embed_one(&dest.content).await?;

                // 5. Add destination to the vector index.
                unit_state
                    .index
                    .add(&unit_to_scope, &vector, dest_qualified)?;

                // 6. Remove the source from the vector index (best-effort — stale
                //    entries are skipped at recall time).
                if let Err(e) = unit_state.index.remove(&unit_from_scope, &source_qualified) {
                    warn!(
                        name = %unit_name,
                        error = %e,
                        "vector removal failed during move; stale source entry will be skipped at recall"
                    );
                }

                Ok(dest)
            })
            .await
            .map_err(ErrorData::from)?;

            info!(
                ms = start.elapsed().as_millis(),
                name = %name,
                new_name = %new_name,
                from_scope = %from_scope,
                to_scope = %to_scope,
                "memory moved"
            );

            Ok(serde_json::json!({
                "id": dest.id,
                "name": dest.name,
                "scope": dest.metadata.scope.to_string(),
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// List stored memories, optionally filtered by scope.
    ///
    /// Returns a bounded JSON page of memory summaries. Full content bodies
    /// are omitted for brevity.
    #[tool(
        name = "list",
        description = "List stored memories. Pass a bare path like '<basename-of-your-cwd>' for that scope + global memories, \
        'global' for global-only, or 'all' for everything. Omitting scope defaults to global-only. \
        Pages are sorted by (scope, name); limit defaults to 50, and values outside 1..=100 are rejected. \
        Continue with the opaque next_cursor returned by the previous page. Concurrent inserts and \
        deletes follow standard keyset semantics. Use fields to request an exact summary \
        projection; omitting fields returns id, name, scope, tags, created_at, and updated_at. \
        The response envelope reports count as the total matching memories and returned as the number \
        in this page, plus limit, has_more, and next_cursor. Successful pages are capped at 24 KiB; \
        request fewer fields if one summary is too large."
    )]
    async fn list(
        &self,
        Parameters(args): Parameters<ListToolArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.list",
            session_id = %session_id,
            scope = ?args.scope,
            count = tracing::field::Empty,
        );
        let state = Arc::clone(&self.state);
        async move {
            let limit = validate_list_limit(args.limit).map_err(ErrorData::from)?;
            let scope_filter =
                ScopeFilter::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;
            let filter_key = list_filter_key(&scope_filter);
            let cursor = args
                .cursor
                .as_deref()
                .map(|cursor| decode_list_cursor(cursor, &filter_key))
                .transpose()
                .map_err(ErrorData::from)?;
            let fields = args.fields.unwrap_or_else(|| ListField::ALL.to_vec());

            let start = Instant::now();
            let memories = match &scope_filter {
                ScopeFilter::RootOnly => state
                    .router
                    .list_memories(Some(&Scope::Root))
                    .await
                    .map_err(ErrorData::from)?,
                ScopeFilter::All => state
                    .router
                    .list_memories(None)
                    .await
                    .map_err(ErrorData::from)?,
                ScopeFilter::Subtree(sp) => {
                    let path_scope = Scope::Path(sp.clone());
                    let mut root_memories = state
                        .router
                        .list_memories(Some(&Scope::Root))
                        .await
                        .map_err(ErrorData::from)?;
                    let path_memories = state
                        .router
                        .list_memories(Some(&path_scope))
                        .await
                        .map_err(ErrorData::from)?;
                    root_memories.extend(path_memories);
                    root_memories
                }
            };
            let count = memories.len();
            tracing::Span::current().record("count", count);
            info!(ms = start.elapsed().as_millis(), count, "listed memories");
            paginate_list(memories, &scope_filter, limit, cursor, &fields).map_err(ErrorData::from)
        }
        .instrument(span)
        .await
    }

    /// Read the full content of a specific memory by name.
    ///
    /// Returns the memory's markdown content (frontmatter stripped) plus
    /// metadata (id, scope, tags, timestamps) as a JSON object.
    #[tool(
        name = "read",
        description = "Read a specific memory by name. Use a bare path scope like '<basename-of-your-cwd>' for \
        scoped memories or omit for global. Returns the full markdown content and metadata \
        (id, scope, tags, timestamps) as a JSON object. Set straddle=true to read the same \
        name from every configured sibling store with per-store provenance and partial-failure status."
    )]
    async fn read(
        &self,
        Parameters(args): Parameters<ReadToolArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let name = MemoryName::new(args.name).map_err(ErrorData::from)?;
        let session_id = extract_session_id(&parts);
        let bearer = args.straddle.then(|| extract_bearer(&parts)).flatten();
        let span = tracing::info_span!(
            "handler.read",
            session_id = %session_id,
            name = %name,
            scope = ?args.scope,
            straddle = args.straddle,
        );
        let state = Arc::clone(&self.state);
        let federation = self.federation.clone();
        async move {
            let scope = Scope::parse_or_default(args.scope.as_deref()).map_err(ErrorData::from)?;

            let start = Instant::now();
            if args.straddle {
                let scope_label = scope.to_string();
                let (local, mut stores) = tokio::join!(
                    state.router.read_memory(&name, &scope),
                    federation.read_siblings(&name, &scope_label, bearer),
                );
                let local = match local {
                    Ok(memory) => StoreReadResult::found(memory_value(memory)),
                    Err(MemoryError::NotFound { .. }) => {
                        StoreReadResult::status(StoreReadStatus::NotFound)
                    }
                    Err(error) => {
                        warn!(error = %error, "local store failed during straddled read");
                        StoreReadResult::status(StoreReadStatus::Unreachable)
                    }
                };
                let degraded =
                    local.is_degraded() || stores.values().any(StoreReadResult::is_degraded);
                stores.insert(federation.store_id().to_owned(), local);
                info!(
                    ms = start.elapsed().as_millis(),
                    name = %name,
                    stores = stores.len(),
                    degraded,
                    "read memory across stores"
                );
                return Ok(serde_json::json!({
                    "name": name,
                    "scope": scope_label,
                    "stores": stores,
                    "degraded": degraded,
                })
                .to_string());
            }

            let memory = state
                .router
                .read_memory(&name, &scope)
                .await
                .map_err(ErrorData::from)?;
            info!(
                ms = start.elapsed().as_millis(),
                name = %name,
                "read memory"
            );

            Ok(memory_value(memory).to_string())
        }
        .instrument(span)
        .await
    }

    /// Synchronise the memory repository with the configured git remote.
    ///
    /// Optionally pulls before pushing (default: true). Requires a GitHub
    /// token configured via `MEMORY_MCP_GITHUB_TOKEN` or
    /// `~/.config/memory-mcp/token`.
    ///
    /// Returns a status message describing what happened.
    #[tool(
        name = "sync",
        description = "Sync the memory repo with the git remote (push/pull). Requires \
        MEMORY_MCP_GITHUB_TOKEN or a token file. Returns a status message."
    )]
    async fn sync(
        &self,
        Parameters(args): Parameters<SyncArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let pull_first = args.pull_first.unwrap_or(true);
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.sync",
            session_id = %session_id,
            pull_first,
        );
        let state = Arc::clone(&self.state);
        async move {
            let start = Instant::now();
            let branch = state.branch.clone();

            // Router-wide pull/push plus every derived-index mirror is one
            // cancellation-shielded mutation unit. A disconnected requester
            // cannot leave one mapped repo's pull committed but unmirrored.
            let unit_state = Arc::clone(&state);
            let unit_branch = branch.clone();
            let (sync_result, total_reindex, any_reindex) =
                shielded_mutation_unit(&state.lexical, async move {
                    let sync_result = unit_state
                        .router
                        .sync_all(&unit_state.auth, &unit_branch, pull_first)
                        .await?;
                    let (total_reindex, any_reindex) =
                        mirror_sync_results(&unit_state, &sync_result).await;
                    Ok((sync_result, total_reindex, any_reindex))
                })
                .await
                .map_err(ErrorData::from)?;

            // A failed pull or push on any repo must fail the tool call —
            // successful repos were still synced and mirrored above, but a
            // clean "sync complete" here would silently drop the failures.
            if !sync_result.all_ok() {
                warn!(
                    ms = start.elapsed().as_millis(),
                    pull_first, "sync finished with per-repo failures"
                );
                return Err(sync_failure_error(&sync_result, &branch));
            }

            // Build pull status summary.
            let pull_status = if !pull_first {
                "skipped".to_string()
            } else if sync_result.results.len() == 1 {
                match &sync_result.results[0].pull {
                    Some(PullResult::NoRemote) => "no-remote".to_string(),
                    Some(PullResult::UpToDate) => "up-to-date".to_string(),
                    Some(PullResult::FastForward { .. }) => "fast-forward".to_string(),
                    Some(PullResult::Merged {
                        conflicts_resolved, ..
                    }) => {
                        format!("merged ({} conflicts resolved)", conflicts_resolved)
                    }
                    None => "skipped".to_string(),
                }
            } else {
                format!("{} repos synced", sync_result.results.len())
            };

            info!(
                ms = start.elapsed().as_millis(),
                pull_first,
                pull_status = %pull_status,
                "sync complete"
            );

            let mut response = serde_json::json!({
                "status": "sync complete",
                "pull": pull_status,
                "branch": branch,
            });

            if any_reindex {
                response["reindex"] = serde_json::json!({
                    "added": total_reindex.added,
                    "updated": total_reindex.updated,
                    "removed": total_reindex.removed,
                    "errors": total_reindex.errors,
                });
            }

            Ok(response.to_string())
        }
        .instrument(span)
        .await
    }

    /// Return recall precision statistics bucketed by distance range.
    ///
    /// Reads all rows in the recall event log and groups them into 0.05-wide
    /// distance buckets. Each bucket reports applied / maybe / not_applied /
    /// unknown counts, which agents can use to calibrate their recall distance
    /// Report that a recalled memory influenced this session.
    ///
    /// Writes the agent's feedback back to the recall log — whether the memory
    /// was applied, an optional note about how it was used, and a confidence
    /// level. This data feeds into threshold calibration via `recall-stats`.
    ///
    /// Returns a JSON object with `rows_affected`.
    #[tool(
        name = "mark_applied",
        description = "Report whether a recalled memory was useful. Call for each memory in the \
        recall results after you have decided whether to use it. Pass verdict='applied' if it \
        influenced the session, 'maybe' if partially relevant, or 'not_applied' if not relevant. \
        This bidirectional feedback calibrates recall thresholds. Pass the recall_id from the \
        recall response, the memory name, the verdict, an optional application note, and a \
        confidence level ('high', 'medium', or 'low')."
    )]
    async fn mark_applied(
        &self,
        Parameters(args): Parameters<MarkAppliedArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.mark_applied",
            session_id = %session_id,
            recall_id = %args.recall_id,
            memory = %args.memory,
            verdict = %args.verdict,
        );
        let state = Arc::clone(&self.state);
        async move {
            match args.confidence.as_str() {
                "high" | "medium" | "low" => {}
                other => {
                    return Err(ErrorData::from(MemoryError::InvalidInput {
                        reason: format!(
                            "invalid confidence '{}'; expected 'high', 'medium', or 'low'",
                            other
                        ),
                    }));
                }
            }

            let rows_affected = if let Some(ref log) = state.recall_log {
                let log = Arc::clone(log);
                let recall_id = args.recall_id.clone();
                let memory = args.memory.clone();
                let sid = session_id.clone();
                let verdict = args.verdict.as_str();
                let application = args.application.clone();
                let confidence = args.confidence.clone();
                traced_spawn_blocking(move || {
                    log.mark_applied(
                        &recall_id,
                        &memory,
                        &sid,
                        verdict,
                        application.as_deref(),
                        &confidence,
                    )
                })
                .await
                .map_err(|e| {
                    ErrorData::from(MemoryError::Internal(format!("spawn_blocking: {e}")))
                })?
                .map_err(ErrorData::from)?
            } else {
                warn!("mark_applied called but recall log is not enabled");
                0
            };

            if rows_affected == 0 {
                warn!(
                    recall_id = %args.recall_id,
                    memory = %args.memory,
                    "mark_applied matched no rows"
                );
            }

            info!(
                rows_affected,
                memory = %args.memory,
                verdict = %args.verdict,
                "mark_applied"
            );

            Ok(serde_json::json!({
                "rows_affected": rows_affected,
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// Report verdicts for multiple recalled memories in a single call.
    ///
    /// Accepts an array of verdict entries and processes them in one database
    /// transaction. Each entry follows the same semantics as `mark_applied`:
    /// first call wins per (`recall_id`, `memory_name`, `session_id`) tuple.
    ///
    /// Returns per-entry row counts and an aggregate total.
    #[tool(
        name = "batch_mark_applied",
        description = "Report verdicts for multiple recalled memories in a single call. Pass an \
        array of verdict objects, each with recall_id, memory name, verdict ('applied', 'maybe', \
        or 'not_applied'), optional application note, and confidence level. Processes all verdicts \
        in one transaction. Use this instead of calling mark_applied repeatedly — it reduces \
        round-trips from N calls to 1."
    )]
    async fn batch_mark_applied(
        &self,
        Parameters(args): Parameters<BatchMarkAppliedArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let session_id = extract_session_id(&parts);
        let count = args.verdicts.len();
        let span = tracing::info_span!(
            "handler.batch_mark_applied",
            session_id = %session_id,
            count,
        );
        let state = Arc::clone(&self.state);
        async move {
            if args.verdicts.is_empty() {
                return Err(ErrorData::from(MemoryError::InvalidInput {
                    reason: "verdicts array must not be empty".into(),
                }));
            }

            // Validate all confidence values up front.
            for (i, entry) in args.verdicts.iter().enumerate() {
                match entry.confidence.as_str() {
                    "high" | "medium" | "low" => {}
                    other => {
                        return Err(ErrorData::from(MemoryError::InvalidInput {
                            reason: format!(
                                "invalid confidence '{}' at index {}; expected 'high', 'medium', or 'low'",
                                other, i
                            ),
                        }));
                    }
                }
            }

            let total_rows = if let Some(ref log) = state.recall_log {
                let log = Arc::clone(log);
                let sid = session_id.clone();

                // Build owned copies directly from the deserialized args so we
                // can move them into spawn_blocking.
                let owned: Vec<OwnedBatchVerdict> = args
                    .verdicts
                    .iter()
                    .map(|v| OwnedBatchVerdict {
                        recall_id: v.recall_id.clone(),
                        memory_name: v.memory.clone(),
                        verdict: v.verdict.as_str().to_owned(),
                        application_note: v.application.clone(),
                        confidence: v.confidence.clone(),
                    })
                    .collect();

                let per_entry = traced_spawn_blocking(move || {
                    let refs: Vec<BatchVerdict<'_>> = owned
                        .iter()
                        .map(|o| BatchVerdict {
                            recall_id: &o.recall_id,
                            memory_name: &o.memory_name,
                            verdict: &o.verdict,
                            application_note: o.application_note.as_deref(),
                            confidence: &o.confidence,
                        })
                        .collect();
                    log.batch_mark_applied(&sid, &refs)
                })
                .await
                .map_err(|e| {
                    ErrorData::from(MemoryError::Internal(format!("spawn_blocking: {e}")))
                })?
                .map_err(ErrorData::from)?;

                let total: u64 = per_entry.iter().sum();

                // Warn for entries that matched nothing.
                for (i, &rows) in per_entry.iter().enumerate() {
                    if rows == 0 {
                        warn!(
                            recall_id = %args.verdicts[i].recall_id,
                            memory = %args.verdicts[i].memory,
                            index = i,
                            "batch_mark_applied entry matched no rows"
                        );
                    }
                }

                info!(
                    total_rows = total,
                    entries = per_entry.len(),
                    "batch_mark_applied"
                );

                total
            } else {
                warn!("batch_mark_applied called but recall log is not enabled");
                0
            };

            Ok(serde_json::json!({
                "total_rows_affected": total_rows,
                "entries_processed": args.verdicts.len(),
            })
            .to_string())
        }
        .instrument(span)
        .await
    }

    /// Return recall precision statistics bucketed by distance range.
    ///
    /// Agents can use this to inspect their own recall quality and adjust
    /// thresholds.
    ///
    /// Returns an error if the recall log is not enabled.
    #[tool(
        name = "recall_stats",
        description = "Return recall precision statistics bucketed by distance range. Shows applied/maybe/not_applied/unknown counts per bucket, useful for calibrating recall thresholds. No arguments required."
    )]
    async fn recall_stats(
        &self,
        Parameters(_args): Parameters<RecallStatsArgs>,
        Extension(parts): Extension<http::request::Parts>,
    ) -> Result<String, ErrorData> {
        let session_id = extract_session_id(&parts);
        let span = tracing::info_span!(
            "handler.recall_stats",
            session_id = %session_id,
        );
        let state = Arc::clone(&self.state);
        async move {
            if state.recall_log.is_none() {
                return Err(ErrorData::from(MemoryError::Internal(
                    "recall log is not enabled".to_string(),
                )));
            }
            let buckets = crate::repo::traced_spawn_blocking(move || {
                // Safety: checked above that recall_log is Some.
                state
                    .recall_log
                    .as_ref()
                    .expect("recall_log is Some — checked above")
                    .recall_stats()
            })
            .await
            .map_err(|e| ErrorData::from(MemoryError::Internal(format!("spawn_blocking: {e}"))))?
            .map_err(ErrorData::from)?;

            let json_buckets: Vec<serde_json::Value> = buckets
                .iter()
                .filter(|b| b.total > 0)
                .map(|b| {
                    serde_json::json!({
                        "range": format!("{:.2}–{:.2}", b.range_start, b.range_end),
                        "total": b.total,
                        "applied": b.applied,
                        "maybe": b.maybe,
                        "not_applied": b.not_applied,
                        "unknown": b.unknown,
                    })
                })
                .collect();

            info!(buckets = json_buckets.len(), "recall_stats");

            Ok(serde_json::json!({
                "buckets": json_buckets,
            })
            .to_string())
        }
        .instrument(span)
        .await
    }
}

#[tool_handler]
impl ServerHandler for MemoryServer {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let tool_name = request.name.to_string();
        let context = ToolCallContext::new(self, request, context);
        finish_tool_call(&tool_name, start, self.tool_router.call(context).await)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "A semantic memory system for AI coding agents. Memories are stored as markdown files \
            in a git repository and indexed for semantic retrieval. Use `remember` to store, `recall` \
            to search, `read` to fetch a specific memory, `edit` to update, `forget` to delete, \
            `list` to browse, and `sync` to push/pull the remote.\n\n\
            Scope convention: pass a bare namespace path like 'my-project' or 'org/team' when \
            working within a namespace. Paths may be hierarchical (e.g. 'org/team/project'). \
            This returns that namespace's memories alongside global ones. \
            Omitting scope defaults to global-only for queries (recall, list) and targets a single \
            memory for point operations (remember, edit, read, forget). Use scope='all' to search \
            across all scopes.\n\n\
            IMPORTANT: Never store credentials, API keys, tokens, passwords, or other secrets in \
            memory content. Memories are stored as plaintext markdown files committed to a git \
            repository and may be synced to a remote. Treat all memory content as public.\n\n\
            Recall feedback: after acting on recalled memories, call `mark_applied` for each result \
            with your verdict ('applied', 'maybe', or 'not_applied'). Every recall response includes \
            a `recall_id` — pass it back with each verdict. This bidirectional feedback calibrates \
            recall thresholds. Use `batch_mark_applied` to submit multiple verdicts in a single call \
            instead of calling `mark_applied` repeatedly. Use `recall_stats` to inspect precision \
            by distance bucket.\n\n\
            Breaking change (v0.14.0): the 'project:<name>' scope format is no longer accepted \
            as tool input. If your prompts or configuration pass scope values like \
            'project:my-project', replace them with the bare path: 'my-project'. The 'global' \
            scope is unchanged. Stored memories are unaffected — old scope formats in YAML \
            frontmatter are read correctly without migration."
                .to_string(),
        )
    }
}

/// Sentinel `distance` for hits that only the lexical strategy returned.
///
/// Lexical-only hits have no embedding distance, but the `distance` field
/// predates hybrid retrieval and was always numeric on the wire — strict
/// clients deserialize it as a required float, so it must stay numeric
/// (never `null` or absent). `-1.0` is impossible as a real cosine distance
/// and matches the recall log's lexical-only sentinel.
const LEXICAL_ONLY_DISTANCE_SENTINEL: f64 = -1.0;

/// Build one recall result entry as seen on the wire.
///
/// `distance` is the semantic (cosine) distance, or the numeric sentinel
/// [`LEXICAL_ONLY_DISTANCE_SENTINEL`] (`-1.0`) for hits that only the
/// lexical strategy returned. `match_type` says which strategies
/// contributed: `"semantic"`, `"lexical"`, or `"both"`.
fn recall_entry_json(memory: &Memory, hit: &FusedHit) -> serde_json::Value {
    let (snippet, content_length, truncated) = build_snippet(&memory.content);
    serde_json::json!({
        "id": memory.id,
        "name": memory.name,
        "scope": memory.metadata.scope.to_string(),
        "tags": memory.metadata.tags,
        "content": snippet,
        "content_length": content_length,
        "truncated": truncated,
        "distance": hit
            .semantic_distance
            .map_or(LEXICAL_ONLY_DISTANCE_SENTINEL, f64::from),
        "match_type": hit.match_type(),
    })
}

/// Truncate content to [`SNIPPET_MAX_CHARS`] and return `(snippet, content_length, truncated)`.
fn build_snippet(content: &str) -> (String, usize, bool) {
    let content_length = content.chars().count();
    let truncated = content_length > SNIPPET_MAX_CHARS;
    let snippet: String = content.chars().take(SNIPPET_MAX_CHARS).collect();
    (snippet, content_length, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_log::capture_info_logs;
    use crate::{auth::AuthProvider, health::HealthRegistry, index::InMemoryStore};
    use async_trait::async_trait;
    use rmcp::model::Content;
    use std::sync::Arc;

    struct ListTestEmbedding;

    #[async_trait]
    impl EmbeddingBackend for ListTestEmbedding {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
            Ok(vec![vec![0.0; 4]; texts.len()])
        }

        fn dimensions(&self) -> usize {
            4
        }
    }

    #[test]
    fn tool_timing_preserves_content_and_adds_metadata() {
        let original = CallToolResult::success(vec![Content::text("{\"ok\":true}")]);
        let original_content = original.content.clone();

        let result = finish_tool_call("read", Instant::now(), Ok(original)).expect("success");

        assert_eq!(result.content, original_content);
        assert_eq!(result.structured_content, None);
        assert_eq!(result.is_error, Some(false));
        let duration = result
            .meta
            .as_ref()
            .and_then(|meta| meta.get(SERVER_PROCESSING_DURATION_META_KEY))
            .and_then(serde_json::Value::as_u64);
        assert!(duration.is_some());
    }

    #[test]
    fn tool_timing_merges_existing_metadata() {
        let mut meta = Meta::new();
        meta.insert("existing".to_string(), serde_json::json!("kept"));
        let original = CallToolResult::success(vec![Content::text("ok")]).with_meta(Some(meta));

        let result = finish_tool_call("list", Instant::now(), Ok(original)).expect("success");
        let meta = result.meta.expect("metadata");

        assert_eq!(meta.get("existing"), Some(&serde_json::json!("kept")));
        assert!(meta.contains_key(SERVER_PROCESSING_DURATION_META_KEY));
    }

    #[test]
    fn tool_timing_preserves_error_data_semantics() {
        let original =
            ErrorData::invalid_params("bad input", Some(serde_json::json!({"field": "name"})));
        let expected_code = original.code;
        let expected_message = original.message.clone();
        let expected_data = original.data.clone();

        let error = finish_tool_call("edit", Instant::now(), Err(original)).expect_err("error");

        assert_eq!(error.code, expected_code);
        assert_eq!(error.message, expected_message);
        assert_eq!(error.data, expected_data);
    }

    #[test]
    fn tool_timing_logs_success_and_error_at_info() {
        let logs = capture_info_logs(|| {
            let success = CallToolResult::success(vec![Content::text("ok")]);
            finish_tool_call("read", Instant::now(), Ok(success)).expect("success");
            let error = ErrorData::invalid_params("bad input", None);
            finish_tool_call("edit", Instant::now(), Err(error)).expect_err("error");
        });

        assert!(logs.contains("tool_name=\"read\""), "logs: {logs}");
        assert!(logs.contains("tool_name=\"edit\""), "logs: {logs}");
        assert!(logs.contains("outcome=\"success\""), "logs: {logs}");
        assert!(logs.contains("outcome=\"error\""), "logs: {logs}");
        assert!(
            logs.contains("server_processing_duration_ms="),
            "logs: {logs}"
        );
    }

    #[test]
    fn edit_stage_timing_logs_success_and_failure_at_info() {
        let logs = capture_info_logs(|| {
            let mut success = EditStageTiming::new();
            success.read_ms = 1;
            success.embed_total_ms = 2;
            success.index_ms = 3;
            success.repo_save_ms = 4;
            success.completed();
            drop(success);

            let mut failure = EditStageTiming::new();
            failure.stage = "embed";
            failure.read_ms = 1;
            failure.embed_total_ms = 2;
            drop(failure);
        });

        assert!(logs.contains("stage=\"completed\""), "logs: {logs}");
        assert!(logs.contains("stage=\"embed\""), "logs: {logs}");
        assert!(logs.contains("outcome=\"success\""), "logs: {logs}");
        assert!(logs.contains("outcome=\"error\""), "logs: {logs}");
        for field in [
            "edit_total_ms=",
            "read_ms=",
            "embed_total_ms=",
            "index_ms=",
            "repo_save_ms=",
        ] {
            assert!(logs.contains(field), "missing {field}; logs: {logs}");
        }
        assert!(
            !logs.contains("server_processing_duration_ms="),
            "edit-stage logs must not reuse the authoritative tool-boundary field; logs: {logs}"
        );
    }

    /// Regression guard: `note` must stay registered alongside `remember`
    /// with a byte-identical input schema, so callers can use either name
    /// interchangeably.
    #[test]
    fn note_is_registered_with_remember_schema() {
        let router = MemoryServer::tool_router();
        let tools = router.list_all();
        let find = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("tool {name} not registered"))
        };
        let remember = find("remember");
        let note = find("note");
        assert_eq!(note.input_schema, remember.input_schema);
    }

    #[test]
    fn bearer_extraction_accepts_only_nonempty_bearer_credentials() {
        let parts_with = |value: &str| {
            http::Request::builder()
                .header(http::header::AUTHORIZATION, value)
                .body(())
                .unwrap()
                .into_parts()
                .0
        };

        assert_eq!(
            extract_bearer(&parts_with("Bearer opaque-token")).as_deref(),
            Some("opaque-token")
        );
        assert_eq!(
            extract_bearer(&parts_with("bearer second-token")).as_deref(),
            Some("second-token")
        );
        assert!(extract_bearer(&parts_with("Basic nope")).is_none());
        assert!(extract_bearer(&parts_with("Bearer   ")).is_none());
    }

    #[test]
    fn snippet_short_content_not_truncated() {
        let content = "Hello, world!";
        let (snippet, content_length, truncated) = build_snippet(content);
        assert_eq!(snippet, "Hello, world!");
        assert_eq!(content_length, 13);
        assert!(!truncated);
    }

    #[test]
    fn snippet_exact_limit_not_truncated() {
        let content: String = "a".repeat(SNIPPET_MAX_CHARS);
        let (snippet, content_length, truncated) = build_snippet(&content);
        assert_eq!(snippet, content);
        assert_eq!(content_length, SNIPPET_MAX_CHARS);
        assert!(!truncated);
    }

    #[test]
    fn snippet_over_limit_is_truncated() {
        let content: String = "b".repeat(SNIPPET_MAX_CHARS + 100);
        let (snippet, content_length, truncated) = build_snippet(&content);
        assert_eq!(snippet.chars().count(), SNIPPET_MAX_CHARS);
        assert_eq!(content_length, SNIPPET_MAX_CHARS + 100);
        assert!(truncated);
    }

    #[test]
    fn snippet_counts_unicode_chars_not_bytes() {
        // Each emoji is 1 char but multiple bytes.
        let emoji_content: String = "\u{1F600}".repeat(SNIPPET_MAX_CHARS + 1);
        let (snippet, content_length, truncated) = build_snippet(&emoji_content);
        assert_eq!(snippet.chars().count(), SNIPPET_MAX_CHARS);
        assert_eq!(content_length, SNIPPET_MAX_CHARS + 1);
        assert!(truncated);
    }

    #[test]
    fn snippet_empty_content() {
        let (snippet, content_length, truncated) = build_snippet("");
        assert_eq!(snippet, "");
        assert_eq!(content_length, 0);
        assert!(!truncated);
    }

    fn list_test_memory(name: &str, scope: Scope, tags: Vec<String>) -> Memory {
        Memory::new(name, "body", MemoryMetadata::new(scope, tags, None)).expect("valid memory")
    }

    fn list_test_path(path: &str) -> Scope {
        Scope::Path(crate::types::ScopePath::new(path).expect("valid scope"))
    }

    fn list_test_server(repo: Arc<MemoryRepo>) -> MemoryServer {
        let state = Arc::new(AppState::new(
            repo,
            "main".to_string(),
            Box::new(ListTestEmbedding),
            Box::new(InMemoryStore::new(4)),
            AuthProvider::new(),
            HealthRegistry::new(),
            None,
        ));
        MemoryServer::new(state)
    }

    fn list_test_parts() -> http::request::Parts {
        http::Request::builder()
            .body(())
            .expect("request")
            .into_parts()
            .0
    }

    #[test]
    fn list_limit_rejects_zero_and_values_above_maximum() {
        assert!(matches!(
            validate_list_limit(Some(0)),
            Err(MemoryError::InvalidInput { .. })
        ));
        assert!(matches!(
            validate_list_limit(Some(LIST_MAX_LIMIT + 1)),
            Err(MemoryError::InvalidInput { .. })
        ));
        assert_eq!(
            validate_list_limit(None).expect("default limit"),
            LIST_DEFAULT_LIMIT
        );
    }

    #[tokio::test]
    async fn list_mcp_handler_rejects_invalid_limit_and_cursor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = Arc::new(MemoryRepo::init_or_open(temp.path(), None).expect("repo"));
        let server = list_test_server(repo);

        let invalid_limit = server
            .list(
                Parameters(ListToolArgs {
                    scope: None,
                    limit: Some(0),
                    cursor: None,
                    fields: None,
                }),
                Extension(list_test_parts()),
            )
            .await
            .expect_err("zero limit must fail");
        assert_eq!(invalid_limit.code, rmcp::model::ErrorCode::INVALID_PARAMS);

        let invalid_cursor = server
            .list(
                Parameters(ListToolArgs {
                    scope: None,
                    limit: None,
                    cursor: Some("garbage".to_string()),
                    fields: None,
                }),
                Extension(list_test_parts()),
            )
            .await
            .expect_err("malformed cursor must fail");
        assert_eq!(invalid_cursor.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn list_mcp_handler_walks_seeded_repo_across_separate_reads() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = Arc::new(MemoryRepo::init_or_open(temp.path(), None).expect("repo"));
        for name in ["delta", "alpha", "charlie"] {
            repo.save_memory(&list_test_memory(name, Scope::Root, vec![]))
                .await
                .expect("seed memory");
        }
        let server = list_test_server(Arc::clone(&repo));
        let mut cursor = None;
        let mut walked = Vec::new();

        loop {
            let page = server
                .list(
                    Parameters(ListToolArgs {
                        scope: None,
                        limit: Some(1),
                        cursor,
                        fields: Some(vec![ListField::Name]),
                    }),
                    Extension(list_test_parts()),
                )
                .await
                .expect("list page");
            let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
            walked.push(
                page["memories"][0]["name"]
                    .as_str()
                    .expect("memory name")
                    .to_string(),
            );
            cursor = page["next_cursor"].as_str().map(str::to_string);
            if walked.len() == 1 {
                repo.save_memory(&list_test_memory("bravo", Scope::Root, vec![]))
                    .await
                    .expect("concurrent insert");
                repo.delete_memory("alpha", &Scope::Root)
                    .await
                    .expect("delete cursor anchor");
            }
            if cursor.is_none() {
                break;
            }
        }

        assert_eq!(walked, ["alpha", "bravo", "charlie", "delta"]);
    }

    #[tokio::test]
    async fn list_mcp_handler_applies_default_limit_of_fifty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = Arc::new(MemoryRepo::init_or_open(temp.path(), None).expect("repo"));
        for index in 0..51 {
            repo.save_memory(&list_test_memory(
                &format!("memory-{index:02}"),
                Scope::Root,
                vec![],
            ))
            .await
            .expect("seed memory");
        }
        let page = list_test_server(repo)
            .list(
                Parameters(ListToolArgs {
                    scope: None,
                    limit: None,
                    cursor: None,
                    fields: Some(vec![ListField::Name]),
                }),
                Extension(list_test_parts()),
            )
            .await
            .expect("default-limit page");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(page["count"], 51);
        assert_eq!(page["returned"], LIST_DEFAULT_LIMIT);
        assert_eq!(page["limit"], LIST_DEFAULT_LIMIT);
        assert_eq!(page["has_more"], true);
        assert!(page["next_cursor"].is_string());
    }

    #[test]
    fn list_cursor_rejects_malformed_and_scope_mismatch() {
        let all = ScopeFilter::All;
        let key = ListSortKey {
            scope: "global".to_string(),
            name: "alpha".to_string(),
        };
        let cursor = encode_list_cursor(&list_filter_key(&all), &key).expect("cursor");

        assert!(matches!(
            decode_list_cursor("not-a-cursor", "all"),
            Err(MemoryError::InvalidInput { .. })
        ));
        assert!(matches!(
            decode_list_cursor(&cursor, "root"),
            Err(MemoryError::InvalidInput { .. })
        ));
    }

    #[test]
    fn list_cursor_rejects_untrusted_shapes_with_remediation() {
        let unsupported_version = format!(
            "lc1_{}",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "version": 2,
                    "filter": "all",
                    "scope": "global",
                    "name": "alpha",
                }))
                .expect("cursor payload"),
            )
        );
        let cases = [
            "lc1_".to_string(),
            "lc1_*".to_string(),
            format!("lc1_{}", URL_SAFE_NO_PAD.encode(b"not JSON")),
            unsupported_version,
            "x".repeat(LIST_CURSOR_MAX_CHARS + 1),
        ];

        for cursor in cases {
            let error = decode_list_cursor(&cursor, "all").expect_err("cursor must fail");
            let MemoryError::InvalidInput { reason } = error else {
                panic!("expected invalid input");
            };
            assert!(
                reason.contains("omit cursor to start a new page"),
                "missing remediation: {reason}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Wire-contract tests — the recall result entry shape MCP clients see.
    // -----------------------------------------------------------------------

    fn wire_test_memory() -> Memory {
        let metadata = MemoryMetadata::new(Scope::Root, vec!["tag-a".to_string()], None);
        Memory::new("wire-test", "some content".to_string(), metadata).expect("valid memory")
    }

    fn entry_keys(entry: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = entry
            .as_object()
            .expect("entry must be an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn recall_entry_has_exactly_the_contract_fields() {
        let hit = FusedHit {
            qualified_name: "v1:scope=global;name=wire-test".to_string(),
            semantic_distance: Some(0.25),
            lexical_score: Some(4.2),
            score: 0.03,
        };

        let entry = recall_entry_json(&wire_test_memory(), &hit);

        assert_eq!(
            entry_keys(&entry),
            vec![
                "content",
                "content_length",
                "distance",
                "id",
                "match_type",
                "name",
                "scope",
                "tags",
                "truncated",
            ]
        );
    }

    #[test]
    fn recall_entry_semantic_hit_has_numeric_distance() {
        let hit = FusedHit {
            qualified_name: "v1:scope=global;name=wire-test".to_string(),
            semantic_distance: Some(0.25),
            lexical_score: None,
            score: 0.02,
        };

        let entry = recall_entry_json(&wire_test_memory(), &hit);

        let distance = entry["distance"].as_f64().expect("distance is a number");
        assert!((distance - 0.25).abs() < 1e-6);
        assert_eq!(entry["match_type"], "semantic");
    }

    #[test]
    fn recall_entry_lexical_only_hit_has_numeric_sentinel_distance() {
        let hit = FusedHit {
            qualified_name: "v1:scope=global;name=wire-test".to_string(),
            semantic_distance: None,
            lexical_score: Some(7.5),
            score: 0.016,
        };

        let entry = recall_entry_json(&wire_test_memory(), &hit);

        // The pre-hybrid wire contract has `distance` as a required numeric
        // field; lexical-only hits must keep it numeric (never null/absent)
        // so strict older clients still deserialize the entry.
        let distance = entry["distance"]
            .as_f64()
            .expect("distance must stay numeric for lexical-only hits");
        assert!((distance - LEXICAL_ONLY_DISTANCE_SENTINEL).abs() < 1e-12);
        assert_eq!(entry["match_type"], "lexical");
    }

    #[test]
    fn recall_entry_lexical_only_hit_keeps_the_full_contract_shape() {
        // Both hit shapes — with a real distance and with the sentinel —
        // must expose exactly the same field set.
        let hit = FusedHit {
            qualified_name: "v1:scope=global;name=wire-test".to_string(),
            semantic_distance: None,
            lexical_score: Some(7.5),
            score: 0.016,
        };

        let entry = recall_entry_json(&wire_test_memory(), &hit);

        assert_eq!(
            entry_keys(&entry),
            vec![
                "content",
                "content_length",
                "distance",
                "id",
                "match_type",
                "name",
                "scope",
                "tags",
                "truncated",
            ]
        );
    }

    #[test]
    fn recall_entry_deserializes_for_a_strict_numeric_distance_client() {
        // Simulates a pre-hybrid client that models `distance` as a
        // required f32 — both hit shapes must satisfy it.
        #[derive(serde::Deserialize)]
        struct StrictClientEntry {
            #[allow(dead_code)]
            name: String,
            distance: f32,
        }

        for semantic_distance in [Some(0.25_f32), None] {
            let hit = FusedHit {
                qualified_name: "v1:scope=global;name=wire-test".to_string(),
                semantic_distance,
                lexical_score: Some(7.5),
                score: 0.016,
            };
            let entry = recall_entry_json(&wire_test_memory(), &hit);
            let parsed: StrictClientEntry = serde_json::from_value(entry)
                .expect("strict numeric-distance clients must keep working");
            assert!(
                parsed.distance >= -1.0,
                "sentinel is the only negative value"
            );
        }
    }

    #[test]
    fn list_cursor_encoder_never_emits_a_cursor_the_decoder_rejects_for_size() {
        let round_trip_key = ListSortKey {
            scope: "s".repeat(1_000),
            name: "n".repeat(1_000),
        };
        let cursor = encode_list_cursor("all", &round_trip_key).expect("bounded cursor");
        let decoded = decode_list_cursor(&cursor, "all").expect("cursor round trip");
        assert_eq!(decoded.key, round_trip_key);

        let oversized_key = ListSortKey {
            scope: "s".repeat(LIST_CURSOR_MAX_CHARS),
            name: "n".repeat(LIST_CURSOR_MAX_CHARS),
        };
        assert!(matches!(
            encode_list_cursor("all", &oversized_key),
            Err(MemoryError::InvalidInput { .. })
        ));
    }

    #[test]
    fn list_cursor_round_trips_generated_keys() {
        for length in 0..=128 {
            let key = ListSortKey {
                scope: format!("scope-{}", "é".repeat(length)),
                name: format!("name-{}", "界".repeat(length)),
            };
            let cursor = encode_list_cursor("subtree:scope", &key).expect("encode cursor");
            let decoded = decode_list_cursor(&cursor, "subtree:scope").expect("decode cursor");
            assert_eq!(decoded.key, key);
        }
    }

    #[test]
    fn list_cursor_fits_maximum_filesystem_legal_scope_and_name() {
        let component = "s".repeat(255);
        let scope = std::iter::repeat_n(component.as_str(), 10)
            .collect::<Vec<_>>()
            .join("/");
        let name = std::iter::repeat_n("n".repeat(255), 3)
            .collect::<Vec<_>>()
            .join("/");
        let key = ListSortKey {
            scope: scope.clone(),
            name,
        };
        let filter = format!("subtree:{scope}");
        let cursor = encode_list_cursor(&filter, &key).expect("legal key must fit cursor bound");
        assert!(cursor.len() <= LIST_CURSOR_MAX_CHARS);
        assert_eq!(
            decode_list_cursor(&cursor, &filter).expect("decode").key,
            key
        );
    }

    #[test]
    fn list_paginates_past_a_maximum_legal_interior_key() {
        let component = "s".repeat(255);
        let scope_path = std::iter::repeat_n(component.as_str(), 10)
            .collect::<Vec<_>>()
            .join("/");
        let scope = list_test_path(&scope_path);
        let long_name = std::iter::repeat_n("m".repeat(255), 3)
            .collect::<Vec<_>>()
            .join("/");
        let memories = vec![
            list_test_memory("alpha", scope.clone(), vec![]),
            list_test_memory(&long_name, scope.clone(), vec![]),
            list_test_memory("zulu", scope, vec![]),
        ];
        let filter = ScopeFilter::Subtree(
            crate::types::ScopePath::new(scope_path).expect("valid scope path"),
        );

        let page = paginate_list(memories, &filter, 2, None, &[ListField::Name])
            .expect("long interior key must remain pageable");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(page["returned"], 2);
        let cursor = page["next_cursor"].as_str().expect("next cursor");
        assert_eq!(
            decode_list_cursor(cursor, list_filter_key(&filter).as_ref())
                .expect("decode")
                .key
                .name,
            long_name
        );
    }

    #[test]
    fn list_cursor_tolerates_concurrent_changes_with_keyset_semantics() {
        let original = vec![
            list_test_memory("alpha", Scope::Root, vec![]),
            list_test_memory("charlie", Scope::Root, vec![]),
        ];
        let first = paginate_list(
            original.clone(),
            &ScopeFilter::RootOnly,
            1,
            None,
            &[ListField::Name],
        )
        .expect("first page");
        let first: serde_json::Value = serde_json::from_str(&first).expect("JSON page");
        let cursor = first["next_cursor"]
            .as_str()
            .expect("next cursor")
            .to_string();

        let changed = vec![
            original[0].clone(),
            list_test_memory("aardvark", Scope::Root, vec![]),
            list_test_memory("bravo", Scope::Root, vec![]),
            original[1].clone(),
        ];
        let page = paginate_list(
            changed,
            &ScopeFilter::RootOnly,
            10,
            Some(decode_list_cursor(&cursor, "root").expect("cursor")),
            &[ListField::Name],
        )
        .expect("concurrent changes must not stale the cursor");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(
            page["memories"],
            serde_json::json!([{"name": "bravo"}, {"name": "charlie"}])
        );
    }

    #[test]
    fn list_cursor_tolerates_an_observable_edit_after_the_cursor() {
        let original = vec![
            list_test_memory("alpha", Scope::Root, vec![]),
            list_test_memory("charlie", Scope::Root, vec![]),
        ];
        let first = paginate_list(
            original.clone(),
            &ScopeFilter::RootOnly,
            1,
            None,
            &[ListField::Name],
        )
        .expect("first page");
        let first: serde_json::Value = serde_json::from_str(&first).expect("JSON page");
        let cursor = first["next_cursor"].as_str().expect("cursor");
        let mut edited = original;
        edited[1].metadata.tags.push("edited".to_string());

        let page = paginate_list(
            edited,
            &ScopeFilter::RootOnly,
            10,
            Some(decode_list_cursor(cursor, "root").expect("cursor")),
            &[ListField::Name, ListField::Tags],
        )
        .expect("edit must not stale cursor");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(
            page["memories"],
            serde_json::json!([{"name": "charlie", "tags": ["edited"]}])
        );
    }

    #[test]
    fn list_pages_walk_stably_in_scope_name_order() {
        let memories = vec![
            list_test_memory("zulu", list_test_path("team/b"), vec![]),
            list_test_memory("bravo", Scope::Root, vec![]),
            list_test_memory("alpha", list_test_path("team/a"), vec![]),
            list_test_memory("alpha", Scope::Root, vec![]),
            list_test_memory("charlie", list_test_path("team/a"), vec![]),
        ];
        let fields = [ListField::Scope, ListField::Name];
        let mut cursor = None;
        let mut walked = Vec::new();

        loop {
            let decoded = cursor
                .as_deref()
                .map(|cursor| decode_list_cursor(cursor, "all").expect("cursor"));
            let page = paginate_list(memories.clone(), &ScopeFilter::All, 2, decoded, &fields)
                .expect("page");
            let value: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
            assert_eq!(value["count"], 5);
            assert!(page.len() <= LIST_PAGE_MAX_BYTES);
            for memory in value["memories"].as_array().expect("memories") {
                walked.push(format!(
                    "{}/{}",
                    memory["scope"].as_str().expect("scope"),
                    memory["name"].as_str().expect("name")
                ));
            }
            cursor = value["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        assert_eq!(
            walked,
            [
                "global/alpha",
                "global/bravo",
                "team/a/alpha",
                "team/a/charlie",
                "team/b/zulu",
            ]
        );
    }

    #[test]
    fn list_projection_is_exact_and_omission_preserves_legacy_fields() {
        let memory = list_test_memory("alpha", Scope::Root, vec!["tag".to_string()]);
        let projected = list_summary(&memory, &[ListField::Name]);
        assert_eq!(projected, serde_json::json!({"name": "alpha"}));

        let legacy = list_summary(&memory, &ListField::ALL);
        let object = legacy.as_object().expect("summary object");
        assert_eq!(object.len(), 6);
        for field in ["id", "name", "scope", "tags", "created_at", "updated_at"] {
            assert!(object.contains_key(field), "missing {field}");
        }
    }

    #[test]
    fn list_final_page_has_exact_wire_shape_and_present_null_cursor() {
        let page = paginate_list(
            vec![list_test_memory("alpha", Scope::Root, vec![])],
            &ScopeFilter::RootOnly,
            25,
            None,
            &[ListField::Name],
        )
        .expect("final page");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(
            page,
            serde_json::json!({
                "memories": [{"name": "alpha"}],
                "count": 1,
                "returned": 1,
                "limit": 25,
                "has_more": false,
                "next_cursor": null,
            })
        );
    }

    #[test]
    fn list_nonfinal_page_has_exact_wire_shape() {
        let page = paginate_list(
            vec![
                list_test_memory("alpha", Scope::Root, vec![]),
                list_test_memory("bravo", Scope::Root, vec![]),
            ],
            &ScopeFilter::RootOnly,
            1,
            None,
            &[ListField::Name],
        )
        .expect("nonfinal page");
        let page: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        let cursor = page["next_cursor"].as_str().expect("next cursor");
        assert!(decode_list_cursor(cursor, "root").is_ok());
        assert_eq!(
            page,
            serde_json::json!({
                "memories": [{"name": "alpha"}],
                "count": 2,
                "returned": 1,
                "limit": 1,
                "has_more": true,
                "next_cursor": cursor,
            })
        );
    }

    #[test]
    fn list_empty_scope_has_exact_wire_shape() {
        let page = paginate_list(
            vec![],
            &ScopeFilter::RootOnly,
            LIST_DEFAULT_LIMIT,
            None,
            &[ListField::Name],
        )
        .expect("empty page");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&page).expect("JSON page"),
            serde_json::json!({
                "memories": [],
                "count": 0,
                "returned": 0,
                "limit": 50,
                "has_more": false,
                "next_cursor": null,
            })
        );
    }

    #[test]
    fn list_byte_ceiling_splits_pages_without_losing_memories() {
        let memories: Vec<Memory> = (0..30)
            .map(|index| {
                list_test_memory(
                    &format!("memory-{index:02}"),
                    Scope::Root,
                    vec!["x".repeat(2_000)],
                )
            })
            .collect();
        let mut cursor = None;
        let mut walked = Vec::new();
        let mut pages = 0;

        loop {
            let decoded = cursor
                .as_deref()
                .map(|cursor| decode_list_cursor(cursor, "root").expect("cursor"));
            let page = paginate_list(
                memories.clone(),
                &ScopeFilter::RootOnly,
                LIST_MAX_LIMIT,
                decoded,
                &ListField::ALL,
            )
            .expect("bounded page");
            assert!(page.len() <= LIST_PAGE_MAX_BYTES, "{} bytes", page.len());
            let value: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
            assert_eq!(value["count"], memories.len());
            for memory in value["memories"].as_array().expect("memories") {
                walked.push(memory["name"].as_str().expect("memory name").to_string());
            }
            pages += 1;
            cursor = value["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        let expected: Vec<_> = (0..30).map(|index| format!("memory-{index:02}")).collect();
        assert_eq!(walked, expected);
        assert!(pages > 1);
    }

    #[test]
    fn list_pathological_tags_require_a_leaner_projection() {
        let memories = vec![list_test_memory(
            "huge-tags",
            Scope::Root,
            vec!["x".repeat(LIST_PAGE_MAX_BYTES)],
        )];
        let error = paginate_list(
            memories.clone(),
            &ScopeFilter::RootOnly,
            1,
            None,
            &ListField::ALL,
        )
        .expect_err("legacy projection is too large");
        assert!(matches!(error, MemoryError::InvalidInput { .. }));

        let page = paginate_list(
            memories,
            &ScopeFilter::RootOnly,
            1,
            None,
            &[ListField::Name],
        )
        .expect("lean projection fits");
        assert!(page.len() <= LIST_PAGE_MAX_BYTES);
        let value: serde_json::Value = serde_json::from_str(&page).expect("JSON page");
        assert_eq!(
            value["memories"],
            serde_json::json!([{"name": "huge-tags"}])
        );
    }

    #[test]
    fn recall_entry_both_hit_keeps_semantic_distance() {
        let hit = FusedHit {
            qualified_name: "v1:scope=global;name=wire-test".to_string(),
            semantic_distance: Some(0.31),
            lexical_score: Some(2.0),
            score: 0.033,
        };

        let entry = recall_entry_json(&wire_test_memory(), &hit);

        assert_eq!(entry["match_type"], "both");
        assert!(entry["distance"].is_number());
    }

    // -----------------------------------------------------------------------
    // Lexical failure/repair receipts (#310, ADR-0039)
    //
    // Failure injection across the mutation paths that mirror git writes
    // into the lexical index. Each receipt asserts the contract end to end:
    // the handler stays best-effort (the git write succeeds), the failed
    // mirror flags the index degraded, and the repair loop deterministically
    // converges the index back to git truth.
    // -----------------------------------------------------------------------

    mod lexical_failure_receipts {
        use super::*;
        use crate::auth::AuthProvider;
        use crate::config::RemoteMapping;
        use crate::health::HealthRegistry;
        use crate::index::InMemoryStore;
        use crate::repo::MemoryRepo;
        use crate::repo_router::{RepoRouter, SyncEntry};
        use crate::search::bm25::FailPoint;
        use crate::search::{rebuild_lexical_from_repo, spawn_lexical_repair};
        use async_trait::async_trait;

        struct MockEmbedding;

        #[async_trait]
        impl crate::embedding::EmbeddingBackend for MockEmbedding {
            async fn embed(
                &self,
                texts: &[String],
            ) -> Result<Vec<Vec<f32>>, crate::error::MemoryError> {
                Ok(texts.iter().map(|_| vec![0.0, 0.0, 0.0, 1.0]).collect())
            }

            fn dimensions(&self) -> usize {
                4
            }
        }

        fn parts() -> http::request::Parts {
            http::Request::builder()
                .uri("/")
                .body(())
                .expect("request")
                .into_parts()
                .0
        }

        fn test_state(tmp: &tempfile::TempDir) -> Arc<AppState> {
            let repo = MemoryRepo::init_or_open(tmp.path(), None).expect("repo init");
            Arc::new(AppState::new(
                Arc::new(repo),
                "main".to_string(),
                Box::new(MockEmbedding),
                Box::new(InMemoryStore::new(4)),
                AuthProvider::new(),
                HealthRegistry::new(),
                None,
            ))
        }

        fn qualified(scope: &Scope, name: &str) -> String {
            MemoryRef::new(scope.clone(), MemoryName::new(name.to_string()).unwrap())
                .qualified_path()
        }

        /// Poll until the lexical index is consistent and `query` returns
        /// exactly `expected` — the deterministic-convergence receipt. The
        /// repair task runs in the background, so convergence is eventual
        /// but bounded.
        async fn assert_lexical_converges(state: &Arc<AppState>, query: &str, expected: &[String]) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if !state.lexical.is_degraded() {
                    if let Ok(hits) = state.lexical.search(&ScopeFilter::All, query, 10) {
                        let names: Vec<String> = hits.into_iter().map(|(n, _)| n).collect();
                        if names == expected {
                            return;
                        }
                    }
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "lexical index never converged to git truth for query {query:?} \
                     (expected {expected:?}, degraded={})",
                    state.lexical.is_degraded()
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        async fn remember(
            server: &MemoryServer,
            name: &str,
            content: &str,
        ) -> Result<String, ErrorData> {
            server
                .remember(
                    Parameters(RememberArgs {
                        content: content.to_string(),
                        name: name.to_string(),
                        tags: vec![],
                        scope: None,
                        source: None,
                    }),
                    Extension(parts()),
                )
                .await
        }

        #[tokio::test]
        async fn read_without_straddle_preserves_legacy_response_shape() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));
            remember(&server, "person-cammy", "hello")
                .await
                .expect("remember");

            let response = server
                .read(
                    Parameters(ReadToolArgs {
                        name: "person-cammy".into(),
                        scope: None,
                        straddle: false,
                    }),
                    Extension(parts()),
                )
                .await
                .expect("read");
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();

            assert_eq!(response["content"], "hello");
            assert!(response.get("stores").is_none());
            assert!(response.get("degraded").is_none());
        }

        #[tokio::test]
        async fn straddled_read_without_identity_returns_local_and_marks_siblings() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let config: Config = toml::from_str(
                r#"
store_id = "personal"
[siblings]
fcc = "http://127.0.0.1:9/mcp"
"#,
            )
            .unwrap();
            let server = MemoryServer::with_config(Arc::clone(&state), &config).expect("config");
            remember(&server, "person-cammy", "private hello")
                .await
                .expect("remember");

            let response = server
                .read(
                    Parameters(ReadToolArgs {
                        name: "person-cammy".into(),
                        scope: None,
                        straddle: true,
                    }),
                    Extension(parts()),
                )
                .await
                .expect("straddled read");
            let response: serde_json::Value = serde_json::from_str(&response).unwrap();

            assert_eq!(response["stores"]["personal"]["status"], "found");
            assert_eq!(
                response["stores"]["personal"]["memory"]["content"],
                "private hello"
            );
            assert_eq!(response["stores"]["fcc"]["status"], "identity_unavailable");
            assert_eq!(response["degraded"], true);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn remember_mirror_failure_degrades_then_repairs_to_git_truth() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            state.lexical.fail_next(FailPoint::Commit);
            remember(&server, "note", "rememberword content")
                .await
                .expect("remember must stay best-effort despite the lexical failure");

            // Git truth holds the memory even though the mirror failed.
            state
                .repo
                .read_memory(&MemoryName::new("note".to_string()).unwrap(), &Scope::Root)
                .await
                .expect("git truth must hold the memory");

            assert_lexical_converges(&state, "rememberword", &[qualified(&Scope::Root, "note")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn edit_mirror_failure_degrades_then_repairs_to_git_truth() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "note", "oldword content")
                .await
                .expect("remember");

            state.lexical.fail_next(FailPoint::Commit);
            server
                .edit(
                    Parameters(EditArgs {
                        name: "note".to_string(),
                        content: Some("newword content".to_string()),
                        tags: None,
                        scope: None,
                    }),
                    Extension(parts()),
                )
                .await
                .expect("edit must stay best-effort despite the lexical failure");

            // Post-repair: new content searchable, old content gone.
            assert_lexical_converges(&state, "newword", &[qualified(&Scope::Root, "note")]).await;
            let stale = state
                .lexical
                .search(&ScopeFilter::All, "oldword", 10)
                .expect("post-repair search");
            assert!(stale.is_empty(), "stale content survived repair: {stale:?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn forget_mirror_failure_degrades_then_repairs_to_git_truth() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "doomed", "doomedword content")
                .await
                .expect("remember");

            state.lexical.fail_next(FailPoint::Commit);
            server
                .forget(
                    Parameters(ForgetArgs {
                        name: "doomed".to_string(),
                        scope: None,
                    }),
                    Extension(parts()),
                )
                .await
                .expect("forget must stay best-effort despite the lexical failure");

            // Post-repair: the deleted memory is not lexically reachable.
            assert_lexical_converges(&state, "doomedword", &[]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn move_mirror_failure_degrades_then_repairs_to_git_truth() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "wanderer", "moveword content")
                .await
                .expect("remember");

            state.lexical.fail_next(FailPoint::Commit);
            server
                .move_memory(
                    Parameters(MoveArgs {
                        name: "wanderer".to_string(),
                        from_scope: None,
                        to_scope: "proj".to_string(),
                        new_name: None,
                    }),
                    Extension(parts()),
                )
                .await
                .expect("move must stay best-effort despite the lexical failure");

            // Post-repair: only the destination entry exists.
            let dest_scope = Scope::parse_or_default(Some("proj")).unwrap();
            assert_lexical_converges(&state, "moveword", &[qualified(&dest_scope, "wanderer")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn incremental_reindex_mirror_failure_degrades_then_repairs() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            // A memory lands in the repo out-of-band (as after a pull).
            let memory = Memory::from_validated(
                MemoryName::new("pulled".to_string()).unwrap(),
                "pulledword content".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            let changes = ResolvedChanges {
                upserted: vec![MemoryRef::new(
                    Scope::Root,
                    MemoryName::new("pulled".to_string()).unwrap(),
                )],
                removed: vec![],
                unresolved: 0,
            };

            state.lexical.fail_next(FailPoint::Commit);
            let stats = incremental_reindex(
                &state.repo,
                &state.router,
                state.embedding.as_ref(),
                state.index.as_ref(),
                &state.lexical,
                &changes,
            )
            .await;
            assert_eq!(stats.errors, 0, "vector-side reindex must be unaffected");

            assert_lexical_converges(&state, "pulledword", &[qualified(&Scope::Root, "pulled")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn startup_rebuild_failure_degrades_then_retry_converges() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "bootword content".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            // Startup rebuild fails mid-way: index must be degraded, not
            // silently half-built.
            state.lexical.fail_next(FailPoint::BeforeOp(0));
            assert!(rebuild_lexical_from_repo(&state.repo, &state.lexical)
                .await
                .is_err());
            assert!(state.lexical.is_degraded());
            assert!(
                state
                    .lexical
                    .search(&ScopeFilter::All, "bootword", 10)
                    .is_err(),
                "degraded index must not serve results"
            );

            // The retry (same code path as repair) converges.
            rebuild_lexical_from_repo(&state.repo, &state.lexical)
                .await
                .expect("retry rebuild");
            assert!(!state.lexical.is_degraded());
            let hits = state
                .lexical
                .search(&ScopeFilter::All, "bootword", 10)
                .expect("post-repair search");
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].0, qualified(&Scope::Root, "note"));
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn recall_on_degraded_index_serves_semantic_only_and_triggers_repair() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "note", "healme content")
                .await
                .expect("remember");
            state
                .lexical
                .mark_rebuild_required("test: forced divergence");
            assert!(state.lexical.is_degraded());

            // Recall must succeed (semantic-only) while degraded…
            server
                .recall(
                    Parameters(RecallArgs {
                        query: "healme".to_string(),
                        scope: None,
                        limit: Some(5),
                    }),
                    Extension(parts()),
                )
                .await
                .expect("recall must serve semantic-only while degraded");

            // …and it kicks a background repair that converges.
            assert_lexical_converges(&state, "healme", &[qualified(&Scope::Root, "note")]).await;
        }

        // -------------------------------------------------------------------
        // Handler-cancellation receipts (#314 finding 1)
        //
        // The drift window under test: repository commits run in detached
        // `spawn_blocking` work, so a request future dropped at that await
        // point used to let the git write complete while the continuation
        // that mirrors it into the lexical index never ran — leaving the
        // index healthy-but-stale with no repair trigger. Each test drives
        // the real handler, waits until the git commit has *resolved*, then
        // cancels the handler future itself, and asserts the shielded
        // mutation unit still converges the lexical index to git truth.
        // -------------------------------------------------------------------

        /// Poll git truth until `probe` reports the commit has landed, so
        /// the cancellation is gated *after* the blocking repository commit
        /// resolves — the exact window from the finding.
        async fn await_git_truth<F, Fut>(probe: F)
        where
            F: Fn() -> Fut,
            Fut: std::future::Future<Output = bool>,
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !probe().await {
                assert!(
                    std::time::Instant::now() < deadline,
                    "git commit never became observable"
                );
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn remember_cancelled_after_git_commit_still_mirrors() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            let handle =
                tokio::spawn(async move { remember(&server, "note", "cancelword content").await });

            // Gate: the git commit has resolved…
            await_git_truth(|| {
                let state = Arc::clone(&state);
                async move { state.repo.read_memory("note", &Scope::Root).await.is_ok() }
            })
            .await;

            // …now cancel the handler future itself.
            handle.abort();
            let _ = handle.await;

            // The shielded unit still dispatches the mirror: the index
            // converges to git truth instead of staying healthy-but-stale.
            assert_lexical_converges(&state, "cancelword", &[qualified(&Scope::Root, "note")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn edit_cancelled_after_git_commit_still_mirrors() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "note", "oldword content")
                .await
                .expect("remember");
            assert_lexical_converges(&state, "oldword", &[qualified(&Scope::Root, "note")]).await;

            let edit_server = server.clone();
            let handle = tokio::spawn(async move {
                edit_server
                    .edit(
                        Parameters(EditArgs {
                            name: "note".to_string(),
                            content: Some("newword content".to_string()),
                            tags: None,
                            scope: None,
                        }),
                        Extension(parts()),
                    )
                    .await
            });

            await_git_truth(|| {
                let state = Arc::clone(&state);
                async move {
                    state
                        .repo
                        .read_memory("note", &Scope::Root)
                        .await
                        .is_ok_and(|m| m.content == "newword content")
                }
            })
            .await;

            handle.abort();
            let _ = handle.await;

            assert_lexical_converges(&state, "newword", &[qualified(&Scope::Root, "note")]).await;
            let stale = state
                .lexical
                .search(&ScopeFilter::All, "oldword", 10)
                .expect("post-convergence search");
            assert!(stale.is_empty(), "stale content survived: {stale:?}");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn forget_cancelled_after_git_commit_still_mirrors() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "doomed", "doomedword content")
                .await
                .expect("remember");
            assert_lexical_converges(&state, "doomedword", &[qualified(&Scope::Root, "doomed")])
                .await;

            let forget_server = server.clone();
            let handle = tokio::spawn(async move {
                forget_server
                    .forget(
                        Parameters(ForgetArgs {
                            name: "doomed".to_string(),
                            scope: None,
                        }),
                        Extension(parts()),
                    )
                    .await
            });

            await_git_truth(|| {
                let state = Arc::clone(&state);
                async move {
                    state
                        .repo
                        .read_memory("doomed", &Scope::Root)
                        .await
                        .is_err()
                }
            })
            .await;

            handle.abort();
            let _ = handle.await;

            // The deletion must reach the lexical index even though the
            // request was cancelled after the git commit.
            assert_lexical_converges(&state, "doomedword", &[]).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn move_cancelled_after_git_commit_still_mirrors() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            remember(&server, "wanderer", "moveword content")
                .await
                .expect("remember");
            assert_lexical_converges(&state, "moveword", &[qualified(&Scope::Root, "wanderer")])
                .await;

            let move_server = server.clone();
            let handle = tokio::spawn(async move {
                move_server
                    .move_memory(
                        Parameters(MoveArgs {
                            name: "wanderer".to_string(),
                            from_scope: None,
                            to_scope: "proj".to_string(),
                            new_name: None,
                        }),
                        Extension(parts()),
                    )
                    .await
            });

            let dest_scope = Scope::parse_or_default(Some("proj")).unwrap();
            await_git_truth(|| {
                let state = Arc::clone(&state);
                let dest_scope = dest_scope.clone();
                async move {
                    state
                        .repo
                        .read_memory("wanderer", &dest_scope)
                        .await
                        .is_ok()
                }
            })
            .await;

            handle.abort();
            let _ = handle.await;

            // Destination upserted AND source removed — one batch, so
            // convergence to exactly the destination key proves both.
            assert_lexical_converges(&state, "moveword", &[qualified(&dest_scope, "wanderer")])
                .await;
        }

        // -------------------------------------------------------------------
        // Sync fixture: bare origin + writer repo + server state over the
        // same origin, so tests can author pulled changes.
        // -------------------------------------------------------------------

        struct SyncFixture {
            _remote_dir: tempfile::TempDir,
            writer_dir: tempfile::TempDir,
            writer: Arc<MemoryRepo>,
            _server_dir: tempfile::TempDir,
            state: Arc<AppState>,
            server: MemoryServer,
        }

        fn sync_auth() -> AuthProvider {
            AuthProvider::with_token("ghp_fake_token")
        }

        fn sync_fixture() -> SyncFixture {
            let remote_dir = tempfile::tempdir().expect("tempdir");
            git2::Repository::init_bare(remote_dir.path()).expect("bare init");
            let remote_url = format!("file://{}", remote_dir.path().display());

            let writer_dir = tempfile::tempdir().expect("tempdir");
            let writer = Arc::new(
                MemoryRepo::init_or_open(writer_dir.path(), Some(&remote_url))
                    .expect("writer repo"),
            );

            let server_dir = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(
                MemoryRepo::init_or_open(server_dir.path(), Some(&remote_url))
                    .expect("server repo"),
            );
            let state = Arc::new(AppState::new(
                repo,
                "main".to_string(),
                Box::new(MockEmbedding),
                Box::new(InMemoryStore::new(4)),
                sync_auth(),
                HealthRegistry::new(),
                None,
            ));
            let server = MemoryServer::new(Arc::clone(&state));

            SyncFixture {
                _remote_dir: remote_dir,
                writer_dir,
                writer,
                _server_dir: server_dir,
                state,
                server,
            }
        }

        async fn save_to_writer(fx: &SyncFixture, scope: &Scope, name: &str, content: &str) {
            let memory = Memory::from_validated(
                MemoryName::new(name.to_string()).unwrap(),
                content.to_string(),
                MemoryMetadata::new(scope.clone(), vec![], None),
            );
            fx.writer.save_memory(&memory).await.expect("writer save");
        }

        async fn push_writer(fx: &SyncFixture) {
            fx.writer.push(&sync_auth(), "main").await.expect("push");
        }

        /// Commit a raw file into the writer repo, bypassing memory
        /// validation — for authoring pulled files that `save_memory` would
        /// refuse to produce.
        ///
        /// Must be the *last* write before pushing: it opens a fresh
        /// `git2::Repository` handle whose index is read from disk, whereas
        /// a later `save_memory` through the writer's cached handle could
        /// write a tree from a stale index snapshot that drops this file.
        fn raw_commit_to_writer(fx: &SyncFixture, rel_path: &str, content: &str) {
            let repo = git2::Repository::open(fx.writer_dir.path()).expect("open writer");
            let full = fx.writer_dir.path().join(rel_path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(&full, content).expect("write");
            let mut index = repo.index().expect("index");
            index.add_path(std::path::Path::new(rel_path)).expect("add");
            index.write().expect("index write");
            let tree_oid = index.write_tree().expect("write tree");
            let tree = repo.find_tree(tree_oid).expect("tree");
            let sig = git2::Signature::now("test", "test@test.com").expect("sig");
            let parent = repo.head().expect("head").peel_to_commit().expect("commit");
            repo.commit(Some("HEAD"), &sig, &sig, "raw commit", &tree, &[&parent])
                .expect("commit");
        }

        async fn run_sync(server: &MemoryServer) -> Result<String, ErrorData> {
            run_sync_with(server, true).await
        }

        async fn run_sync_with(
            server: &MemoryServer,
            pull_first: bool,
        ) -> Result<String, ErrorData> {
            server
                .sync(
                    Parameters(SyncArgs {
                        pull_first: Some(pull_first),
                    }),
                    Extension(parts()),
                )
                .await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_cancelled_after_pull_commit_still_reindexes() {
            let fx = sync_fixture();
            save_to_writer(&fx, &Scope::Root, "pulled", "syncword content").await;
            push_writer(&fx).await;
            let state = Arc::clone(&fx.state);

            let sync_server = fx.server.clone();
            let handle = tokio::spawn(async move { run_sync(&sync_server).await });

            // Gate: the pull's git commits are on disk…
            await_git_truth(|| {
                let state = Arc::clone(&state);
                async move { state.repo.read_memory("pulled", &Scope::Root).await.is_ok() }
            })
            .await;

            // …now cancel the sync request itself.
            handle.abort();
            let _ = handle.await;

            // The shielded pull+reindex unit still mirrors the pulled memory.
            assert_lexical_converges(&fx.state, "syncword", &[qualified(&Scope::Root, "pulled")])
                .await;
        }

        // -------------------------------------------------------------------
        // Post-pull complete-or-degraded receipts (#314 round three,
        // finding 1)
        //
        // After a pull has moved git truth, the lexical mirror preparation
        // must be complete-or-degraded: hierarchical-scope changes resolve
        // through frontmatter to the canonical key (never ad-hoc path
        // splitting), and every preparation gap — diff failure, unresolvable
        // pulled file, unreadable changed memory — marks the index
        // rebuild-required and schedules deterministic repair instead of
        // committing a reduced mirror as healthy.
        // -------------------------------------------------------------------

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_pulled_hierarchical_scope_edit_targets_the_right_key() {
            let fx = sync_fixture();
            let scope_ab = Scope::parse_or_default(Some("a/b")).unwrap();
            let scope_a = Scope::parse_or_default(Some("a")).unwrap();

            // `projects/a/b/mem.md` is the ambiguous case: path splitting
            // would read it as scope `a`, name `b/mem`. The sibling in
            // scope `a` guards against prefix cross-talk.
            save_to_writer(&fx, &scope_ab, "mem", "hieroldword payload").await;
            save_to_writer(&fx, &scope_a, "sibling", "siblingword payload").await;
            push_writer(&fx).await;

            run_sync(&fx.server).await.expect("first sync");
            assert_lexical_converges(&fx.state, "hieroldword", &[qualified(&scope_ab, "mem")])
                .await;
            assert_lexical_converges(&fx.state, "siblingword", &[qualified(&scope_a, "sibling")])
                .await;

            // Writer edits the hierarchical-scope memory and pushes.
            fx.writer
                .pull(&sync_auth(), "main")
                .await
                .expect("writer pull");
            save_to_writer(&fx, &scope_ab, "mem", "hiernewword payload").await;
            push_writer(&fx).await;

            run_sync(&fx.server).await.expect("second sync");

            // The edit must land on the canonical `a/b` key: new content
            // searchable there, no ghost of the old content under any key.
            assert_lexical_converges(&fx.state, "hiernewword", &[qualified(&scope_ab, "mem")])
                .await;
            let stale = fx
                .state
                .lexical
                .search(&ScopeFilter::All, "hieroldword", 10)
                .expect("post-sync search");
            assert!(
                stale.is_empty(),
                "old content survived under a misresolved key: {stale:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_pulled_hierarchical_scope_delete_removes_the_right_key() {
            let fx = sync_fixture();
            let scope_ab = Scope::parse_or_default(Some("a/b")).unwrap();
            let scope_a = Scope::parse_or_default(Some("a")).unwrap();

            save_to_writer(&fx, &scope_ab, "mem", "doomedhierword payload").await;
            save_to_writer(&fx, &scope_a, "keeper", "keeperword payload").await;
            push_writer(&fx).await;

            run_sync(&fx.server).await.expect("first sync");
            assert_lexical_converges(&fx.state, "doomedhierword", &[qualified(&scope_ab, "mem")])
                .await;

            // Writer deletes the hierarchical-scope memory and pushes.
            fx.writer
                .pull(&sync_auth(), "main")
                .await
                .expect("writer pull");
            fx.writer
                .delete_memory(&MemoryName::new("mem".to_string()).unwrap(), &scope_ab)
                .await
                .expect("writer delete");
            push_writer(&fx).await;

            run_sync(&fx.server).await.expect("second sync");

            // The removal must target the canonical `a/b` key (a misparsed
            // `scope=a;name=b/mem` removal would be a silent no-op, leaving
            // a ghost), and the `a`-scoped sibling must survive.
            assert_lexical_converges(&fx.state, "doomedhierword", &[]).await;
            assert_lexical_converges(&fx.state, "keeperword", &[qualified(&scope_a, "keeper")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_pulled_unresolvable_file_marks_degraded_and_repairs() {
            let fx = sync_fixture();
            save_to_writer(&fx, &Scope::Root, "good", "goodword payload").await;
            raw_commit_to_writer(&fx, "global/broken.md", "not a parseable memory");
            push_writer(&fx).await;

            // Hold the repair slot so the degraded window is observable
            // instead of racing the background repair.
            assert!(fx.state.lexical.try_claim_repair());

            run_sync(&fx.server)
                .await
                .expect("sync stays best-effort for unresolvable pulled files");
            assert!(
                fx.state.lexical.is_degraded(),
                "an unresolvable pulled file must mark the index rebuild-required"
            );

            // Release the slot; repair converges from git truth (the same
            // resolution rules skip the unparseable file).
            fx.state.lexical.finish_repair();
            spawn_lexical_repair(&fx.state.repo, &fx.state.lexical);
            assert_lexical_converges(&fx.state, "goodword", &[qualified(&Scope::Root, "good")])
                .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_pulled_unreadable_memory_marks_degraded_and_repairs() {
            let fx = sync_fixture();
            // A pulled file whose frontmatter scope disagrees with its
            // on-disk location: the resolved reference cannot be read back
            // from disk — the read-failure preparation gap.
            let elsewhere = Scope::parse_or_default(Some("somewhere/else")).unwrap();
            let misplaced = Memory::from_validated(
                MemoryName::new("misplaced".to_string()).unwrap(),
                "misplacedword payload".to_string(),
                MemoryMetadata::new(elsewhere.clone(), vec![], None),
            )
            .to_markdown()
            .expect("markdown");
            save_to_writer(&fx, &Scope::Root, "good", "goodword payload").await;
            raw_commit_to_writer(&fx, "global/misplaced.md", &misplaced);
            push_writer(&fx).await;

            assert!(fx.state.lexical.try_claim_repair());

            run_sync(&fx.server)
                .await
                .expect("sync stays best-effort for unreadable changed memories");
            assert!(
                fx.state.lexical.is_degraded(),
                "an unreadable changed memory must mark the index rebuild-required \
                 instead of committing the reduced batch as healthy"
            );

            // Repair reads the misplaced file through `list_memories`
            // (frontmatter authority), so convergence lands it under its
            // canonical frontmatter key.
            fx.state.lexical.finish_repair();
            spawn_lexical_repair(&fx.state.repo, &fx.state.lexical);
            assert_lexical_converges(
                &fx.state,
                "misplacedword",
                &[qualified(&elsewhere, "misplaced")],
            )
            .await;
            assert_lexical_converges(&fx.state, "goodword", &[qualified(&Scope::Root, "good")])
                .await;
        }

        // -------------------------------------------------------------------
        // Sync failure surfacing (#293 review, finding 1)
        //
        // A failed pull or push must fail the `sync` tool call — per-repo
        // resilience (continue syncing the other repos) must not become an
        // acked-but-lost sync where the caller reads "sync complete" while
        // every push failed.
        // -------------------------------------------------------------------

        /// State whose repo has an origin that cannot be reached, so pull and
        /// push fail at the transport level.
        fn bad_remote_state(tmp: &tempfile::TempDir) -> Arc<AppState> {
            let repo = MemoryRepo::init_or_open(
                tmp.path(),
                Some("file:///nonexistent/pali-test-remote.git"),
            )
            .expect("repo init");
            Arc::new(AppState::new(
                Arc::new(repo),
                "main".to_string(),
                Box::new(MockEmbedding),
                Box::new(InMemoryStore::new(4)),
                sync_auth(),
                HealthRegistry::new(),
                None,
            ))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_push_failure_surfaces_as_tool_error() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = bad_remote_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            // A real local commit, so the push is genuinely attempted.
            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "pushfail payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            let err = run_sync_with(&server, false)
                .await
                .expect_err("a failed push must fail the sync tool call");
            assert!(
                err.message.contains("sync failed for 1/1 repos"),
                "error must name the failed/total counts: {}",
                err.message
            );
            assert!(
                err.message.contains("default: push failed:"),
                "error must enumerate the failing repo and operation: {}",
                err.message
            );
            let data = err.data.expect("structured failure payload");
            assert_eq!(data["failed"][0]["label"], "default");
            assert!(
                data["failed"][0]["push_error"].is_string(),
                "push_error must carry the failure: {data}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_pull_failure_reported_failed_not_skipped() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = bad_remote_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            let err = run_sync_with(&server, true)
                .await
                .expect_err("a failed pull must fail the sync tool call");
            assert!(
                err.message.contains("default: pull failed:"),
                "a failed pull must be reported as failed: {}",
                err.message
            );
            assert!(
                !err.message.contains("skipped"),
                "a failed pull must not be reported as skipped: {}",
                err.message
            );
            let data = err.data.expect("structured failure payload");
            assert!(
                data["failed"][0]["pull_error"].is_string(),
                "pull_error must carry the failure: {data}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn sync_mixed_failure_enumerates_failed_and_succeeded() {
            // Reachable bare origin for the default repo, seeded by a writer.
            let remote_dir = tempfile::tempdir().expect("tempdir");
            git2::Repository::init_bare(remote_dir.path()).expect("bare init");
            let remote_url = format!("file://{}", remote_dir.path().display());
            let writer_dir = tempfile::tempdir().expect("tempdir");
            let writer = Arc::new(
                MemoryRepo::init_or_open(writer_dir.path(), Some(&remote_url))
                    .expect("writer repo"),
            );
            let seed = Memory::from_validated(
                MemoryName::new("seed".to_string()).unwrap(),
                "seedword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            writer.save_memory(&seed).await.expect("writer save");
            writer
                .push(&sync_auth(), "main")
                .await
                .expect("writer push");

            let default_dir = tempfile::tempdir().expect("tempdir");
            let default_repo = Arc::new(
                MemoryRepo::init_or_open(default_dir.path(), Some(&remote_url))
                    .expect("default repo"),
            );

            // Mapped repo whose origin cannot be reached: its pull fails.
            let work_dir = tempfile::tempdir().expect("tempdir");
            let health = HealthRegistry::new();
            let mapping = RemoteMapping {
                scope: "work".to_string(),
                url: "file:///nonexistent/pali-test-remote.git".to_string(),
                path: Some(work_dir.path().display().to_string()),
                branch: None,
            };
            let router = RepoRouter::from_config(
                Arc::clone(&default_repo),
                std::slice::from_ref(&mapping),
                &health.git,
                &health.sync,
            )
            .expect("router");
            let state = Arc::new(AppState::with_router(
                Arc::clone(&default_repo),
                router,
                "main".to_string(),
                Box::new(MockEmbedding),
                Box::new(InMemoryStore::new(4)),
                sync_auth(),
                health,
                None,
            ));
            let server = MemoryServer::new(Arc::clone(&state));

            let err = run_sync(&server)
                .await
                .expect_err("one failing repo must fail the sync tool call");
            assert!(
                err.message.contains("sync failed for 1/2 repos"),
                "error must show partial failure counts: {}",
                err.message
            );
            assert!(
                err.message.contains("work: pull failed:"),
                "error must name the failing repo: {}",
                err.message
            );
            let data = err.data.expect("structured failure payload");
            assert_eq!(
                data["succeeded"],
                serde_json::json!(["default"]),
                "the repo that synced cleanly must stay visible: {data}"
            );
            assert_eq!(data["failed"][0]["label"], "work");

            // Per-repo resilience held: the default repo pulled the seeded
            // memory despite the mapped repo's failure.
            state
                .repo
                .read_memory("seed", &Scope::Root)
                .await
                .expect("default repo must sync despite the mapped repo failing");
        }

        /// Wire-contract receipt for the sync failure payload: message and
        /// structured `data` shapes MCP clients parse.
        #[test]
        fn sync_failure_error_wire_shape() {
            let result = MultiSyncResult {
                results: vec![
                    SyncEntry {
                        label: "default".to_string(),
                        scope: Scope::Root,
                        pull: Some(PullResult::UpToDate),
                        pull_error: None,
                        push_ok: true,
                        push_error: None,
                        changes: None,
                        resolved_changes: None,
                        changes_complete: true,
                    },
                    SyncEntry {
                        label: "work".to_string(),
                        scope: Scope::Root,
                        pull: None,
                        pull_error: Some("git error: boom".to_string()),
                        push_ok: false,
                        push_error: None,
                        changes: None,
                        resolved_changes: None,
                        changes_complete: true,
                    },
                ],
            };

            let err = sync_failure_error(&result, "main");
            assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
            assert_eq!(
                err.message,
                "sync failed for 1/2 repos: work: pull failed: git error: boom"
            );
            assert_eq!(
                err.data.expect("data"),
                serde_json::json!({
                    "status": "sync failed",
                    "branch": "main",
                    "failed": [{"label": "work", "pull_error": "git error: boom"}],
                    "succeeded": ["default"],
                })
            );
        }

        // -------------------------------------------------------------------
        // Composite-SHA advancement refusal (#293 review, finding 2)
        //
        // The production mirror seam: an entry whose post-pull change
        // discovery did not complete must degrade the lexical index and
        // refuse to advance the stored composite SHA, so the next startup
        // reindexes from git truth.
        // -------------------------------------------------------------------

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn incomplete_change_discovery_refuses_composite_sha_advancement() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            // Git truth exists, so head_sha() is Some and advancement is
            // genuinely possible — the refusal below is the differentiator.
            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "shaword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            // Hold the repair slot so the degraded window is observable.
            assert!(state.lexical.try_claim_repair());

            let entry = |changes_complete: bool| SyncEntry {
                label: "default".to_string(),
                scope: Scope::Root,
                pull: None,
                pull_error: None,
                push_ok: true,
                push_error: None,
                changes: None,
                resolved_changes: None,
                changes_complete,
            };

            let incomplete = MultiSyncResult {
                results: vec![entry(false)],
            };
            let (_stats, any_reindex) = mirror_sync_results(&state, &incomplete).await;
            assert!(!any_reindex);
            assert!(
                state.lexical.is_degraded(),
                "incomplete change discovery must mark the index rebuild-required"
            );
            assert_eq!(
                state.index.commit_sha(),
                None,
                "incomplete change discovery must refuse composite-SHA advancement"
            );

            // Sticky refusal (#293 round 2): a later complete mirror only
            // covers its own pulled range and cannot backfill the gap left
            // by the incomplete discovery, so it must not advance the SHA
            // either — only the next startup's full reindex re-earns it.
            let complete = MultiSyncResult {
                results: vec![entry(true)],
            };
            let _ = mirror_sync_results(&state, &complete).await;
            assert_eq!(
                state.index.commit_sha(),
                None,
                "a complete mirror after an earlier gap must keep refusing \
                 composite-SHA advancement"
            );

            // Contrast on a gap-free state: a complete entry advances the
            // SHA, proving the refusals above came from the incomplete
            // discovery, not a missing head.
            let fresh_tmp = tempfile::tempdir().expect("tempdir");
            let fresh = test_state(&fresh_tmp);
            fresh.repo.save_memory(&memory).await.expect("save");
            let complete = MultiSyncResult {
                results: vec![entry(true)],
            };
            let _ = mirror_sync_results(&fresh, &complete).await;
            let expected = fresh.router.head_sha().await;
            assert!(expected.is_some(), "git truth must yield a head SHA");
            assert_eq!(
                fresh.index.commit_sha(),
                expected,
                "a complete mirror must advance the composite SHA"
            );
        }

        // -------------------------------------------------------------------
        // Partial-rebuild certification (#293 review, round 3)
        //
        // A rebuild that recorded any item-level error is not a verified
        // mirror — certification requires errors == 0. The startup stamp is
        // withheld and incremental composite-SHA advancement is refused, so
        // the next startup rebuilds from git truth instead of trusting a
        // partially populated index that was stamped as intact.
        // -------------------------------------------------------------------

        /// Embedding backend that fails for content containing
        /// "poisonword": the batch path errors and the per-item fallback
        /// records a partial failure (other items embed fine).
        struct PoisonEmbedding;

        #[async_trait]
        impl crate::embedding::EmbeddingBackend for PoisonEmbedding {
            async fn embed(
                &self,
                texts: &[String],
            ) -> Result<Vec<Vec<f32>>, crate::error::MemoryError> {
                if texts.iter().any(|t| t.contains("poisonword")) {
                    return Err(crate::error::MemoryError::Internal("poisoned embed".into()));
                }
                Ok(texts.iter().map(|_| vec![0.0, 0.0, 0.0, 1.0]).collect())
            }

            fn dimensions(&self) -> usize {
                4
            }
        }

        /// Systemic failure backend used to prove that a queue-timed-out batch is
        /// not multiplied into one retry per memory.
        struct TimeoutEmbedding {
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl crate::embedding::EmbeddingBackend for TimeoutEmbedding {
            async fn embed(
                &self,
                _texts: &[String],
            ) -> Result<Vec<Vec<f32>>, crate::error::MemoryError> {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(crate::error::MemoryError::EmbeddingQueueTimeout { timeout_secs: 30.0 })
            }

            fn dimensions(&self) -> usize {
                4
            }
        }

        /// A large active inference can exceed its deadline even though each
        /// item succeeds alone. Reindex must retain the bounded split fallback
        /// for this recoverable case.
        struct BatchOnlyTimeoutEmbedding {
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl crate::embedding::EmbeddingBackend for BatchOnlyTimeoutEmbedding {
            async fn embed(
                &self,
                texts: &[String],
            ) -> Result<Vec<Vec<f32>>, crate::error::MemoryError> {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if texts.len() > 1 {
                    return Err(crate::error::MemoryError::EmbeddingInferenceTimeout {
                        timeout_secs: 30.0,
                    });
                }
                Ok(vec![vec![0.0, 0.0, 0.0, 1.0]])
            }

            fn dimensions(&self) -> usize {
                4
            }
        }

        /// The batch appears splittable, but the first singleton exposes that
        /// the original inference still owns the worker. Remaining retries
        /// must be abandoned rather than forming a new timeout cascade.
        struct SplitThenSystemicEmbedding {
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl crate::embedding::EmbeddingBackend for SplitThenSystemicEmbedding {
            async fn embed(
                &self,
                texts: &[String],
            ) -> Result<Vec<Vec<f32>>, crate::error::MemoryError> {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if texts.len() > 1 {
                    Err(crate::error::MemoryError::EmbeddingInferenceTimeout { timeout_secs: 30.0 })
                } else {
                    Err(crate::error::MemoryError::EmbeddingQueueTimeout { timeout_secs: 30.0 })
                }
            }

            fn dimensions(&self) -> usize {
                4
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn active_inference_timeout_splits_into_recoverable_items() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("repo init"));
            let router = RepoRouter::single(Arc::clone(&repo));
            for name in ["first", "second"] {
                repo.save_memory(&Memory::from_validated(
                    MemoryName::new(name.to_string()).unwrap(),
                    format!("{name} payload"),
                    MemoryMetadata::new(Scope::Root, vec![], None),
                ))
                .await
                .expect("save");
            }
            let embedding = BatchOnlyTimeoutEmbedding {
                calls: std::sync::atomic::AtomicUsize::new(0),
            };
            let index = InMemoryStore::new(4);

            let stats = full_reindex(&router, &embedding, &index)
                .await
                .expect("reindex returns item-level stats");

            assert_eq!(stats.added, 2);
            assert_eq!(stats.errors, 0);
            assert_eq!(
                embedding.calls.load(std::sync::atomic::Ordering::Relaxed),
                3,
                "one batch attempt plus one bounded retry per item"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn full_reindex_stops_split_after_systemic_singleton_failure() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("repo init"));
            let router = RepoRouter::single(Arc::clone(&repo));
            for i in 0..65 {
                let name = format!("memory-{i:02}");
                repo.save_memory(&Memory::from_validated(
                    MemoryName::new(name.clone()).unwrap(),
                    format!("{name} payload"),
                    MemoryMetadata::new(Scope::Root, vec![], None),
                ))
                .await
                .expect("save");
            }
            let embedding = SplitThenSystemicEmbedding {
                calls: std::sync::atomic::AtomicUsize::new(0),
            };
            let index = InMemoryStore::new(4);

            let stats = full_reindex(&router, &embedding, &index)
                .await
                .expect("reindex returns item-level stats");

            assert_eq!(stats.added, 0);
            assert_eq!(stats.errors, 65);
            assert_eq!(
                embedding.calls.load(std::sync::atomic::Ordering::Relaxed),
                2,
                "fallback must stop after the first systemic singleton failure"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn incremental_reindex_stops_split_after_systemic_singleton_failure() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let mut upserted = Vec::new();
            for name in ["first", "second"] {
                state
                    .repo
                    .save_memory(&Memory::from_validated(
                        MemoryName::new(name.to_string()).unwrap(),
                        format!("{name} payload"),
                        MemoryMetadata::new(Scope::Root, vec![], None),
                    ))
                    .await
                    .expect("save");
                upserted.push(MemoryRef::new(
                    Scope::Root,
                    MemoryName::new(name.to_string()).unwrap(),
                ));
            }
            let changes = ResolvedChanges {
                upserted,
                removed: Vec::new(),
                unresolved: 0,
            };
            let embedding = SplitThenSystemicEmbedding {
                calls: std::sync::atomic::AtomicUsize::new(0),
            };

            let stats = incremental_reindex(
                &state.repo,
                &state.router,
                &embedding,
                state.index.as_ref(),
                &state.lexical,
                &changes,
            )
            .await;

            assert_eq!(stats.added, 0);
            assert_eq!(stats.errors, 2);
            assert_eq!(
                embedding.calls.load(std::sync::atomic::Ordering::Relaxed),
                2,
                "fallback must stop after the first systemic singleton failure"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn systemic_full_reindex_failure_does_not_retry_per_item() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("repo init"));
            let router = RepoRouter::single(Arc::clone(&repo));
            for i in 0..65 {
                let name = format!("memory-{i:02}");
                repo.save_memory(&Memory::from_validated(
                    MemoryName::new(name.clone()).unwrap(),
                    format!("{name} payload"),
                    MemoryMetadata::new(Scope::Root, vec![], None),
                ))
                .await
                .expect("save");
            }
            let embedding = TimeoutEmbedding {
                calls: std::sync::atomic::AtomicUsize::new(0),
            };
            let index = InMemoryStore::new(4);

            let stats = full_reindex(&router, &embedding, &index)
                .await
                .expect("reindex returns item-level stats");

            assert_eq!(stats.added, 0);
            assert_eq!(stats.errors, 65);
            assert_eq!(
                embedding.calls.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "a systemic batch timeout must not enqueue per-item duplicates"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn systemic_incremental_reindex_failure_does_not_retry_per_item() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let mut upserted = Vec::new();
            for name in ["first", "second"] {
                state
                    .repo
                    .save_memory(&Memory::from_validated(
                        MemoryName::new(name.to_string()).unwrap(),
                        format!("{name} payload"),
                        MemoryMetadata::new(Scope::Root, vec![], None),
                    ))
                    .await
                    .expect("save");
                upserted.push(MemoryRef::new(
                    Scope::Root,
                    MemoryName::new(name.to_string()).unwrap(),
                ));
            }
            let changes = ResolvedChanges {
                upserted,
                removed: Vec::new(),
                unresolved: 0,
            };
            let embedding = TimeoutEmbedding {
                calls: std::sync::atomic::AtomicUsize::new(0),
            };

            let stats = incremental_reindex(
                &state.repo,
                &state.router,
                &embedding,
                state.index.as_ref(),
                &state.lexical,
                &changes,
            )
            .await;

            assert_eq!(stats.added, 0);
            assert_eq!(stats.errors, 2);
            assert_eq!(
                embedding.calls.load(std::sync::atomic::Ordering::Relaxed),
                1,
                "a systemic incremental timeout must not enqueue per-item duplicates"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn startup_reindex_with_item_errors_withholds_certification() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("repo init"));
            let router = RepoRouter::single(Arc::clone(&repo));

            let memory = |name: &str, content: &str| {
                Memory::from_validated(
                    MemoryName::new(name.to_string()).unwrap(),
                    content.to_string(),
                    MemoryMetadata::new(Scope::Root, vec![], None),
                )
            };
            repo.save_memory(&memory("good", "cleanword payload"))
                .await
                .expect("save good");
            repo.save_memory(&memory("bad", "poisonword payload"))
                .await
                .expect("save bad");
            let head = router.head_sha().await;
            assert!(head.is_some(), "git truth must yield a head SHA");

            // Partial rebuild: one item embeds, one errors. The rebuild
            // "completes" but must NOT be certified.
            let index = InMemoryStore::new(4);
            let certified =
                startup_reindex_and_certify(&router, &PoisonEmbedding, &index, head.as_deref())
                    .await;
            assert!(
                !certified,
                "a rebuild with item-level errors must not be certified"
            );
            assert_eq!(
                index.commit_sha(),
                None,
                "the SHA stamp must be withheld — stamping would make the \
                 next startup skip the rebuild that repairs the gap"
            );

            // Contrast: a clean rebuild of the same truth is certified and
            // stamps the SHA, proving the withheld stamp above came from
            // the item-level error, not a missing head.
            let clean_index = InMemoryStore::new(4);
            let certified =
                startup_reindex_and_certify(&router, &MockEmbedding, &clean_index, head.as_deref())
                    .await;
            assert!(certified, "a clean rebuild must be certified");
            assert_eq!(
                clean_index.commit_sha(),
                head,
                "a clean rebuild must stamp the head SHA"
            );
        }

        // -------------------------------------------------------------------
        // Startup ordering: pull before index construction (#327)
        //
        // Fresh repo path + configured non-empty remote (fresh-clone
        // deployment): the initial pull must complete BEFORE index freshness
        // is decided, so the startup reindex runs against post-pull git
        // truth. Previously the index was rebuilt and certified against the
        // local init commit, and the server reported ready with a semantic
        // index that missed every memory on the remote.
        // -------------------------------------------------------------------

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn startup_prepare_index_pulls_before_freshness_on_fresh_repo() {
            // Non-empty remote: a writer pushes one memory to a bare origin.
            let remote_dir = tempfile::tempdir().expect("tempdir");
            git2::Repository::init_bare(remote_dir.path()).expect("bare init");
            let remote_url = format!("file://{}", remote_dir.path().display());

            let writer_dir = tempfile::tempdir().expect("tempdir");
            let writer = Arc::new(
                MemoryRepo::init_or_open(writer_dir.path(), Some(&remote_url))
                    .expect("writer repo"),
            );
            let pulled = Memory::from_validated(
                MemoryName::new("pulled".to_string()).unwrap(),
                "pulledword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            writer.save_memory(&pulled).await.expect("writer save");
            writer.push(&sync_auth(), "main").await.expect("push");

            // Fresh server path: init_or_open creates a local init commit
            // and configures origin, but never fetches — the fresh-boot
            // branch of the startup sequence.
            let server_dir = tempfile::tempdir().expect("tempdir");
            let repo = Arc::new(
                MemoryRepo::init_or_open(server_dir.path(), Some(&remote_url))
                    .expect("server repo"),
            );
            let router = RepoRouter::single(Arc::clone(&repo));
            let pre_pull_head = router.head_sha().await;
            assert!(pre_pull_head.is_some(), "fresh repo must have init commit");

            // Fresh boot: no persisted index (commit_sha None), so a rebuild
            // runs either way — the regression is WHAT it runs against.
            let auth = sync_auth();
            let (index, reindex_ok) = startup_prepare_index(
                Some((&auth, "main")),
                &router,
                &MockEmbedding,
                Box::new(InMemoryStore::new(4)),
                || Box::new(InMemoryStore::new(4)),
            )
            .await;

            assert!(reindex_ok, "clean rebuild over pulled truth must certify");

            // The pull ran before freshness was decided: HEAD moved past the
            // local init commit and the certified SHA is post-pull truth.
            let head = router.head_sha().await;
            assert!(head.is_some(), "git truth must yield a head SHA");
            assert_ne!(
                head, pre_pull_head,
                "initial pull must advance HEAD past the local init commit"
            );
            assert_eq!(
                index.commit_sha(),
                head,
                "startup must certify the index against post-pull git truth"
            );

            // …and recall sees the pulled memory before ready is signaled:
            // the index handed to the server already contains it.
            let qualified =
                MemoryRef::new(Scope::Root, MemoryName::new("pulled".to_string()).unwrap())
                    .qualified_path();
            assert!(
                index.find_by_name(&qualified).is_some(),
                "pulled memory must be present in the startup index"
            );
            let hits = index
                .search(&ScopeFilter::All, &[0.0, 0.0, 0.0, 1.0], 10)
                .expect("search");
            assert!(
                hits.iter().any(|(_, name, _)| name == &qualified),
                "semantic search must surface the pulled memory, got: {hits:?}"
            );
        }

        // -------------------------------------------------------------------
        // Startup ordering, second lane: scope-mapped remotes (#328 review,
        // round 2)
        //
        // Fresh deployment with a default remote AND a configured
        // scope-mapped remote, both non-empty: the startup pull must cover
        // every routed repo — respecting per-route branch overrides — before
        // the composite-HEAD freshness decision. Pulling only the default
        // repo would certify the mapped repo's empty local-init commit and
        // readiness could go green while every remotely stored mapped-scope
        // memory is absent until an explicit sync.
        // -------------------------------------------------------------------

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn startup_prepare_index_pulls_mapped_remotes_before_freshness() {
            let auth = sync_auth();

            // Default remote: a writer pushes one root-scope memory to main.
            let default_remote_dir = tempfile::tempdir().expect("tempdir");
            git2::Repository::init_bare(default_remote_dir.path()).expect("bare init");
            let default_url = format!("file://{}", default_remote_dir.path().display());
            let default_writer_dir = tempfile::tempdir().expect("tempdir");
            let default_writer = Arc::new(
                MemoryRepo::init_or_open(default_writer_dir.path(), Some(&default_url))
                    .expect("default writer repo"),
            );
            let default_memory = Memory::from_validated(
                MemoryName::new("pulled-default".to_string()).unwrap(),
                "defaultword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            default_writer
                .save_memory(&default_memory)
                .await
                .expect("default writer save");
            default_writer.push(&auth, "main").await.expect("push");

            // Mapped remote: a writer pushes one work-scope memory, then the
            // bare repo's only branch is renamed to `develop` — the memory
            // exists ONLY on the route's override branch, so it is reachable
            // only if the startup pull respects the override.
            let work_scope = Scope::Path(crate::types::ScopePath::new("work").unwrap());
            let work_remote_dir = tempfile::tempdir().expect("tempdir");
            git2::Repository::init_bare(work_remote_dir.path()).expect("bare init");
            let work_url = format!("file://{}", work_remote_dir.path().display());
            let work_writer_dir = tempfile::tempdir().expect("tempdir");
            let work_writer = Arc::new(
                MemoryRepo::init_or_open(work_writer_dir.path(), Some(&work_url))
                    .expect("work writer repo"),
            );
            let work_memory = Memory::from_validated(
                MemoryName::new("pulled-work".to_string()).unwrap(),
                "mappedword payload".to_string(),
                MemoryMetadata::new(work_scope.clone(), vec![], None),
            );
            work_writer
                .save_memory(&work_memory)
                .await
                .expect("work writer save");
            work_writer.push(&auth, "main").await.expect("push");
            git2::Repository::open(work_remote_dir.path())
                .expect("open bare")
                .find_reference("refs/heads/main")
                .expect("main ref")
                .rename("refs/heads/develop", true, "seed develop only")
                .expect("rename to develop");

            // Fresh server side: default repo and mapped repo are both
            // locally initialised, never fetched — the fresh-boot branch of
            // the startup sequence with per-scope remotes configured.
            let server_default_dir = tempfile::tempdir().expect("tempdir");
            let default_repo = Arc::new(
                MemoryRepo::init_or_open(server_default_dir.path(), Some(&default_url))
                    .expect("server default repo"),
            );
            let server_work_dir = tempfile::tempdir().expect("tempdir");
            let health = HealthRegistry::new();
            let mapping = RemoteMapping {
                scope: "work".to_string(),
                url: work_url,
                path: Some(server_work_dir.path().display().to_string()),
                branch: Some("develop".to_string()),
            };
            let router = RepoRouter::from_config(
                default_repo,
                std::slice::from_ref(&mapping),
                &health.git,
                &health.sync,
            )
            .expect("router from config");
            let pre_pull_head = router.head_sha().await;
            assert!(
                pre_pull_head.is_some(),
                "fresh repos must have init commits"
            );

            let (index, reindex_ok) = startup_prepare_index(
                Some((&auth, "main")),
                &router,
                &MockEmbedding,
                Box::new(InMemoryStore::new(4)),
                || Box::new(InMemoryStore::new(4)),
            )
            .await;

            assert!(reindex_ok, "clean rebuild over pulled truth must certify");

            // Every routed repo was pulled before freshness was decided: the
            // composite HEAD moved past the local init commits and the
            // certified SHA is post-pull truth.
            let head = router.head_sha().await;
            assert_ne!(
                head, pre_pull_head,
                "initial pull must advance the composite HEAD past the local init commits"
            );
            assert_eq!(
                index.commit_sha(),
                head,
                "startup must certify the index against post-pull composite git truth"
            );

            // BOTH memories are indexed before ready is signaled.
            for (scope, name) in [(Scope::Root, "pulled-default"), (work_scope, "pulled-work")] {
                let qualified = MemoryRef::new(scope, MemoryName::new(name.to_string()).unwrap())
                    .qualified_path();
                assert!(
                    index.find_by_name(&qualified).is_some(),
                    "{qualified} must be present in the startup index"
                );
                let hits = index
                    .search(&ScopeFilter::All, &[0.0, 0.0, 0.0, 1.0], 10)
                    .expect("search");
                assert!(
                    hits.iter().any(|(_, hit, _)| hit == &qualified),
                    "semantic search must surface {qualified}, got: {hits:?}"
                );
            }

            // A fully clean startup pull settles aggregate sync health ok.
            assert!(
                health.sync.load().healthy,
                "a fully clean initial pull must settle aggregate sync health"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn partial_incremental_reindex_refuses_composite_sha_advancement() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            // Git truth: one readable memory, so head_sha() is Some and
            // advancement is genuinely possible.
            let good = Memory::from_validated(
                MemoryName::new("good".to_string()).unwrap(),
                "goodword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&good).await.expect("save");

            // A pulled change set naming the readable memory plus one that
            // cannot be read back: the reindex partially succeeds and
            // records one item-level error.
            let mref = |name: &str| {
                MemoryRef::new(Scope::Root, MemoryName::new(name.to_string()).unwrap())
            };
            let result = MultiSyncResult {
                results: vec![SyncEntry {
                    label: "default".to_string(),
                    scope: Scope::Root,
                    pull: None,
                    pull_error: None,
                    push_ok: true,
                    push_error: None,
                    changes: None,
                    resolved_changes: Some(ResolvedChanges {
                        upserted: vec![mref("good"), mref("ghost")],
                        removed: vec![],
                        unresolved: 0,
                    }),
                    changes_complete: true,
                }],
            };
            let (stats, any_reindex) = mirror_sync_results(&state, &result).await;
            assert!(any_reindex);
            assert_eq!(stats.added, 1, "the readable memory must still be mirrored");
            assert_eq!(
                stats.errors, 1,
                "the unreadable memory must be recorded as an item-level error"
            );
            assert!(
                !state.index_mirror_is_intact(),
                "an item-level error must open a sticky mirror gap"
            );
            assert_eq!(
                state.index.commit_sha(),
                None,
                "a partially successful reindex must refuse composite-SHA \
                 advancement — partial success is not certification"
            );
        }

        // -------------------------------------------------------------------
        // Shutdown persistence vs the incomplete-mirror SHA guard
        // (#293 review, round 2)
        //
        // Graceful shutdown persists the vector index. It must never stamp
        // the router's current HEAD over a SHA the mirror refused to
        // advance: doing so would make the next startup see index SHA ==
        // HEAD and skip the reindex that repairs the incomplete mirror.
        // -------------------------------------------------------------------

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shutdown_persistence_refuses_unverified_head_stamp() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            let entry = |changes_complete: bool| SyncEntry {
                label: "default".to_string(),
                scope: Scope::Root,
                pull: None,
                pull_error: None,
                push_ok: true,
                push_error: None,
                changes: None,
                resolved_changes: None,
                changes_complete,
            };

            // Verified state at A: git truth exists and a complete mirror
            // stamps the composite SHA.
            let note_a = Memory::from_validated(
                MemoryName::new("note-a".to_string()).unwrap(),
                "payload a".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&note_a).await.expect("save a");
            let complete = MultiSyncResult {
                results: vec![entry(true)],
            };
            let _ = mirror_sync_results(&state, &complete).await;
            let sha_a = state.index.commit_sha().expect("verified SHA at A");

            // Git advances A → B, but post-pull change discovery for that
            // range fails: the mirror refuses advancement, SHA stays at A.
            let note_b = Memory::from_validated(
                MemoryName::new("note-b".to_string()).unwrap(),
                "payload b".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&note_b).await.expect("save b");
            let incomplete = MultiSyncResult {
                results: vec![entry(false)],
            };
            let _ = mirror_sync_results(&state, &incomplete).await;
            assert_eq!(
                state.index.commit_sha().as_deref(),
                Some(sha_a.as_str()),
                "precondition: the refused advancement must leave the SHA at A"
            );
            let head_b = state.router.head_sha().await.expect("git HEAD at B");
            assert_ne!(head_b, sha_a, "git must have advanced past A");

            // The graceful-shutdown persistence path must keep the verified
            // SHA rather than stamping the current HEAD.
            let index_dir = tempfile::tempdir().expect("index dir");
            persist_index_on_shutdown(&state, index_dir.path())
                .await
                .expect("persist");
            assert_eq!(
                state.index.commit_sha().as_deref(),
                Some(sha_a.as_str()),
                "shutdown must persist only the last verified SHA — stamping \
                 HEAD would make the next startup skip the repairing reindex"
            );
            assert_ne!(
                state.index.commit_sha(),
                state.router.head_sha().await,
                "the persisted SHA must mismatch HEAD so the next startup's \
                 freshness check triggers a full reindex"
            );
        }

        /// The stamp still happens when no mirror gap was recorded: ordinary
        /// writes advance HEAD while mirroring themselves, and shutdown
        /// stamping is what lets the next startup skip a redundant reindex.
        /// Regression guard for over-tightening the shutdown fix.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn shutdown_persistence_stamps_head_when_mirror_intact() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");
            assert_eq!(state.index.commit_sha(), None);

            let index_dir = tempfile::tempdir().expect("index dir");
            persist_index_on_shutdown(&state, index_dir.path())
                .await
                .expect("persist");
            let expected = state.router.head_sha().await;
            assert!(expected.is_some(), "git truth must yield a head SHA");
            assert_eq!(
                state.index.commit_sha(),
                expected,
                "an intact mirror must stamp HEAD at shutdown so the next \
                 startup can skip the reindex"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn post_pull_diff_failure_marks_degraded_and_schedules_repair() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "diffword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            // Hold the repair slot so the degraded window is observable.
            assert!(state.lexical.try_claim_repair());

            // Bogus OIDs: the post-pull diff itself fails after git truth
            // has (nominally) moved.
            let result = mirror_pulled_changes(&state, [0x11; 20], [0x22; 20]).await;
            assert!(result.is_err(), "diff failure must propagate");
            assert!(
                state.lexical.is_degraded(),
                "a post-pull diff failure must mark the index rebuild-required"
            );

            state.lexical.finish_repair();
            spawn_lexical_repair(&state.repo, &state.lexical);
            assert_lexical_converges(&state, "diffword", &[qualified(&Scope::Root, "note")]).await;
        }

        // -------------------------------------------------------------------
        // Pre-list startup failure (#314 finding 2)
        // -------------------------------------------------------------------

        /// A repository-listing failure *before* the `rebuild_from` seam must
        /// not leave a fresh (0/0-epoch) index falsely healthy: the index is
        /// marked degraded, search errors instead of serving healthy-empty
        /// results, and recall schedules the repair that converges.
        #[cfg(unix)]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn startup_list_failure_marks_fresh_index_degraded_and_recall_repairs() {
            use std::os::unix::fs::PermissionsExt;

            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);
            let server = MemoryServer::new(Arc::clone(&state));

            // Git truth exists out-of-band (no lexical mirror yet).
            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "listword content".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            // Make the startup listing fail before the rebuild seam.
            let global_dir = tmp.path().join("global");
            let original = std::fs::metadata(&global_dir)
                .expect("metadata")
                .permissions();
            std::fs::set_permissions(&global_dir, std::fs::Permissions::from_mode(0o000))
                .expect("chmod");
            if std::fs::read_dir(&global_dir).is_ok() {
                // Permissions are not enforced (e.g. running as root) — this
                // seam cannot be exercised here.
                std::fs::set_permissions(&global_dir, original).expect("chmod restore");
                return;
            }

            let result = rebuild_lexical_from_repo(&state.repo, &state.lexical).await;
            std::fs::set_permissions(&global_dir, original).expect("chmod restore");
            assert!(result.is_err(), "listing failure must propagate");

            // The fresh index must NOT stay falsely healthy at 0/0 epochs.
            assert!(
                state.lexical.is_degraded(),
                "a pre-list startup failure must mark the index degraded"
            );
            assert!(
                state
                    .lexical
                    .search(&ScopeFilter::All, "listword", 10)
                    .is_err(),
                "degraded index must error instead of serving healthy-empty results"
            );

            // Recall observes the degraded flag, serves semantic-only, and
            // schedules the repair…
            server
                .recall(
                    Parameters(RecallArgs {
                        query: "listword".to_string(),
                        scope: None,
                        limit: Some(5),
                    }),
                    Extension(parts()),
                )
                .await
                .expect("recall must serve semantic-only while degraded");

            // …which converges to git truth.
            assert_lexical_converges(&state, "listword", &[qualified(&Scope::Root, "note")]).await;
        }

        // -------------------------------------------------------------------
        // Cancellation-independent panic supervision (#314 round three,
        // finding 2)
        //
        // A panic at an unknown point must be observed by the detached work
        // itself — never only by the (cancellable) request future. Each test
        // abandons the requester first and then proves the divergence is
        // still recorded.
        // -------------------------------------------------------------------

        /// Poll until the lexical index reports degraded.
        async fn await_degraded(state: &Arc<AppState>) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !state.lexical.is_degraded() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the divergence was never recorded — no independent observer \
                     marked the index rebuild-required"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn mutation_unit_panic_after_git_commit_with_aborted_request_marks_degraded() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            let (committed_tx, committed_rx) = tokio::sync::oneshot::channel::<()>();
            let release = Arc::new(tokio::sync::Notify::new());

            let unit_state = Arc::clone(&state);
            let unit_release = Arc::clone(&release);
            let requester_lexical = Arc::clone(&state.lexical);
            let requester = tokio::spawn(async move {
                shielded_mutation_unit::<(), _>(&requester_lexical, async move {
                    // Real git commit — the unit dies *after* truth moved.
                    let memory = Memory::from_validated(
                        MemoryName::new("committed".to_string()).unwrap(),
                        "panicunitword payload".to_string(),
                        MemoryMetadata::new(Scope::Root, vec![], None),
                    );
                    unit_state.repo.save_memory(&memory).await?;
                    let _ = committed_tx.send(());
                    unit_release.notified().await;
                    panic!("injected panic after the git commit");
                })
                .await
            });

            // Gate: the git commit has resolved…
            committed_rx.await.expect("git commit signal");
            // …the request future is aborted (no surviving requester)…
            requester.abort();
            let _ = requester.await;
            // …and only now does the detached unit panic.
            release.notify_one();

            // The Drop-guard owned by the detached task must record the
            // divergence with no requester left to observe a JoinError.
            await_degraded(&state).await;

            // Git truth holds the memory the panic stranded.
            state
                .repo
                .read_memory("committed", &Scope::Root)
                .await
                .expect("git truth must hold the committed memory");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn apply_worker_panic_marks_degraded_without_a_surviving_awaiter() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = test_state(&tmp);

            // Git truth exists so post-repair convergence is meaningful.
            let memory = Memory::from_validated(
                MemoryName::new("note".to_string()).unwrap(),
                "applypanicword payload".to_string(),
                MemoryMetadata::new(Scope::Root, vec![], None),
            );
            state.repo.save_memory(&memory).await.expect("save");

            // Dispatch a batch whose blocking worker panics, and abandon the
            // returned future immediately — the eager dispatch means the
            // worker still runs, and the panic must be observed without any
            // surviving awaiter to see the JoinError.
            state.lexical.fail_next(FailPoint::Panic);
            let fut = state
                .lexical
                .apply_async(vec![LexicalOp::Upsert(LexicalDoc {
                    qualified_name: qualified(&Scope::Root, "note"),
                    name: "note".to_string(),
                    content: "applypanicword payload".to_string(),
                })]);
            drop(fut);

            await_degraded(&state).await;

            // Deterministic repair converges from git truth.
            spawn_lexical_repair(&state.repo, &state.lexical);
            assert_lexical_converges(&state, "applypanicword", &[qualified(&Scope::Root, "note")])
                .await;
        }
    }
}
