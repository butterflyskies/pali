//! Access fixture: today's behaviour of every scope-sensitive tool, recorded
//! as the "before" picture for multitenancy work (issue #408).
//!
//! The corpus under `tests/fixtures/access/corpus/` is a fictional memory
//! store in pali's on-disk layout. `tests/fixtures/access/cases.toml` lists
//! operation x requester view x input cases. This test loads the corpus into
//! a fresh repository, drives each case through the real MCP tool handlers
//! in-process (stateless JSON mode over `tower::ServiceExt::oneshot`), and
//! compares the observed results with `tests/fixtures/access/before.json`.
//!
//! The record holds names, scopes, SHA-256 digests of returned bodies,
//! ranking, match types and rounded scores, and for writes the change in
//! repository state. Bodies themselves are never copied into the record.
//!
//! Any behavioural difference fails the test with a per-case line diff.
//! Set `PALI_ACCESS_FIXTURE_RECORD=1` to rewrite the record from the current
//! behaviour instead.
//!
//! Regressions that turn this test red: a scope filter that returns more or
//! fewer memories, a changed ranking or score, a read/write that starts or
//! stops resolving a name in a given scope, or a corpus/case edit that was
//! not re-recorded.

mod common;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::body::Body;
use http::Request;
use pali::{
    auth::AuthProvider,
    health::HealthRegistry,
    index::InMemoryStore,
    repo::MemoryRepo,
    search::{hybrid_search, rebuild_lexical_from_router},
    server::{full_reindex, MemoryServer},
    types::{AppState, Memory, MemoryName, MemoryRef, Scope, ScopeFilter},
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt as _;

use common::StubEmbeddingBackend;

/// Environment variable that switches the test from replay to record.
const RECORD_ENV: &str = "PALI_ACCESS_FIXTURE_RECORD";

/// Version of the record layout in `before.json`.
const RECORD_FORMAT: u64 = 1;

/// Dimensionality of the shared deterministic test embedding.
const DIMS: usize = common::HASH_EMBEDDING_DIMS;

/// Page size used when walking `list` results.
const LIST_PAGE_LIMIT: usize = 100;

/// Fields of tool responses that are deliberately absent from the record,
/// whatever a case's observation projection says: they are generated per
/// run or describe history rather than access behaviour.
const NEVER_RECORDED: &[&str] = &[
    "memory id (uuid)",
    "created_at / updated_at timestamps",
    "recall_id",
    "server processing duration metadata",
    "git commit ids and history",
    "memory bodies (only SHA-256 digests are kept)",
];

/// Write-response keys dropped before recording; the values change every run.
const NEVER_RECORDED_RESPONSE_KEYS: &[&str] = &["id", "created_at", "updated_at", "recall_id"];

/// Default `limit` for recall cases that do not set one.
const DEFAULT_RECALL_LIMIT: usize = 5;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/access")
}

// ---------------------------------------------------------------------------
// Case schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    case: Vec<Case>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Op {
    Read,
    Recall,
    List,
    Remember,
    Edit,
    Move,
    Forget,
}

impl Op {
    fn tool(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Recall => "recall",
            Op::List => "list",
            Op::Remember => "remember",
            Op::Edit => "edit",
            Op::Move => "move",
            Op::Forget => "forget",
        }
    }

    fn is_write(self) -> bool {
        matches!(self, Op::Remember | Op::Edit | Op::Move | Op::Forget)
    }

    fn is_query(self) -> bool {
        matches!(self, Op::Recall | Op::List)
    }
}

/// The requester view a case expresses through today's scope arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum View {
    RootOnly,
    OneScope,
    ScopeAndRoot,
    SeveralScopes,
    All,
}

/// Fictional requesters. A closed set, so a case cannot name anyone else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
enum Principal {
    Ottoline,
    Brannoc,
    Muffet,
    Tamsin,
    Corvane,
    Isolde,
    Peregrine,
    Wexley,
}

/// Proposed outcome under multitenancy. Only `open` exists: no policy
/// choice has been made, so none is asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum After {
    Open,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    query: String,
    scope: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    op: Op,
    view: View,
    principal: Principal,
    intended_grant: String,
    after: After,
    note: String,
    scope: Option<String>,
    name: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    content: Option<String>,
    tags: Option<Vec<String>>,
    tags_all: Option<Vec<String>>,
    from_scope: Option<String>,
    to_scope: Option<String>,
    new_name: Option<String>,
    probe: Option<Probe>,
    observe: Option<Vec<String>>,
}

impl Case {
    fn require<'a>(&self, field: &'a Option<String>, label: &str) -> &'a str {
        field
            .as_deref()
            .unwrap_or_else(|| panic!("case {}: `{label}` is required for {:?}", self.id, self.op))
    }

    /// Check that `view` names the shape today's scope arguments actually
    /// produce, so the view column cannot drift from the request.
    fn validate_view(&self, corpus_scopes: &BTreeSet<String>) {
        let scope_arg = if self.op == Op::Move {
            self.from_scope.as_deref()
        } else {
            self.scope.as_deref()
        };
        let derived = match (scope_arg, self.op.is_query()) {
            (None | Some("global"), _) => View::RootOnly,
            (Some("all"), true) => View::All,
            (Some(_), false) => View::OneScope,
            (Some(path), true) => {
                let prefix = format!("{path}/");
                if corpus_scopes.iter().any(|s| s.starts_with(&prefix)) {
                    View::SeveralScopes
                } else {
                    View::ScopeAndRoot
                }
            }
        };
        assert_eq!(
            self.view, derived,
            "case {}: view does not match the scope argument {scope_arg:?} for {:?}",
            self.id, self.op
        );
        assert!(
            !self.intended_grant.trim().is_empty() && !self.note.trim().is_empty(),
            "case {}: intended_grant and note must not be empty",
            self.id
        );
        // No multitenancy policy has been decided, so every proposed outcome
        // stays open; an asserted outcome needs a schema change first.
        assert_eq!(self.after, After::Open, "case {}: after", self.id);
        assert!(
            self.probe.is_none() || self.op.is_write(),
            "case {}: probe is only meaningful after a write",
            self.id
        );
        // Only the query tools take a tag filter; anywhere else it would be
        // accepted and silently dropped.
        assert!(
            self.tags_all.is_none() || self.op.is_query(),
            "case {}: tags_all is only passed to recall and list",
            self.id
        );
        if let Some(observe) = &self.observe {
            assert!(
                !observe.is_empty(),
                "case {}: observe must name at least one field (omit it to compare the full record)",
                self.id
            );
        }
    }

    /// The observation projection: dotted field paths into the `before`
    /// record that replay compares, or `None` for the full record.
    fn observe_paths(&self) -> Option<Vec<Vec<&str>>> {
        self.observe
            .as_ref()
            .map(|paths| paths.iter().map(|p| p.split('.').collect()).collect())
    }
}

fn load_cases(corpus_scopes: &BTreeSet<String>) -> Vec<Case> {
    let raw = std::fs::read_to_string(fixture_dir().join("cases.toml")).expect("read cases.toml");
    let file: CaseFile = toml::from_str(&raw).expect("parse cases.toml");
    let mut ids = BTreeSet::new();
    for case in &file.case {
        assert!(ids.insert(case.id.clone()), "duplicate case id {}", case.id);
        case.validate_view(corpus_scopes);
    }
    file.case
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `(scope, name)` → `(body digest, tags)` for every memory.
type Snapshot = BTreeMap<(String, String), (String, Vec<String>)>;

fn snapshot(memories: &[Memory]) -> Snapshot {
    memories
        .iter()
        .map(|m| {
            (
                (m.metadata.scope.to_string(), m.name.to_string()),
                (sha256_hex(&m.content), m.metadata.tags.clone()),
            )
        })
        .collect()
}

fn corpus_record(memories: &[Memory]) -> Value {
    Value::Array(
        snapshot(memories)
            .into_iter()
            .map(|((scope, name), (digest, tags))| {
                json!({ "scope": scope, "name": name, "body_sha256": digest, "tags": tags })
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// World: a loaded repository behind the real MCP service
// ---------------------------------------------------------------------------

struct World {
    tmp: tempfile::TempDir,
    state: Arc<AppState>,
    router: axum::Router,
}

async fn build_world(corpus: &[Memory]) -> World {
    let tmp = tempfile::tempdir().expect("temp dir");
    let repo = Arc::new(MemoryRepo::init_or_open(tmp.path(), None).expect("init repo"));
    for memory in corpus {
        repo.save_memory(memory).await.expect("save corpus memory");
    }
    let state = Arc::new(AppState::new(
        repo,
        "main".to_string(),
        Box::new(StubEmbeddingBackend),
        Box::new(InMemoryStore::new(DIMS)),
        AuthProvider::new(),
        HealthRegistry::new(),
        None,
    ));
    let stats = full_reindex(
        &state.router,
        state.embedding.as_ref(),
        state.index.as_ref(),
    )
    .await
    .expect("vector reindex");
    assert_eq!(stats.errors, 0, "vector reindex reported errors");
    rebuild_lexical_from_router(&state.router, &state.lexical)
        .await
        .expect("lexical rebuild");

    // Stateless JSON mode: each tools/call is one self-contained POST that
    // reaches the same handler code as a session-bound call.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true);
    let service_state = Arc::clone(&state);
    let service = StreamableHttpService::new(
        move || Ok(MemoryServer::new(Arc::clone(&service_state))),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    World { tmp, state, router }
}

enum ToolOutcome {
    Ok(Value),
    Err { code: i64, message: String },
}

impl World {
    async fn call(&self, tool: &str, arguments: Value) -> ToolOutcome {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(http::header::HOST, "localhost")
            .header(http::header::ACCEPT, "application/json, text/event-stream")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("valid request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("service call");
        assert_eq!(
            response.status(),
            http::StatusCode::OK,
            "tool {tool}: HTTP status"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let envelope: Value = serde_json::from_slice(&bytes).expect("JSON-RPC response");
        if let Some(error) = envelope.get("error") {
            return ToolOutcome::Err {
                code: error["code"].as_i64().expect("error code"),
                message: self.scrub(error["message"].as_str().unwrap_or_default()),
            };
        }
        let result = &envelope["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        if result["isError"].as_bool() == Some(true) {
            return ToolOutcome::Err {
                code: 0,
                message: self.scrub(text),
            };
        }
        ToolOutcome::Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::from(text)))
    }

    /// Remove the temp-dir path from messages so the record is stable.
    fn scrub(&self, message: &str) -> String {
        message.replace(&self.tmp.path().display().to_string(), "<repo>")
    }

    async fn snapshot(&self) -> Snapshot {
        snapshot(
            &self
                .state
                .repo
                .list_memories(None)
                .await
                .expect("list repository truth"),
        )
    }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

fn round(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    (value * factor).round() / factor
}

fn error_record(code: i64, message: String) -> Value {
    json!({ "status": "error", "code": code, "message": message })
}

fn put(args: &mut Value, key: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        args[key] = value.into();
    }
}

async fn record_read(world: &World, case: &Case) -> Value {
    let mut args = json!({ "name": case.require(&case.name, "name") });
    put(&mut args, "scope", case.scope.clone());
    match world.call("read", args).await {
        ToolOutcome::Ok(v) => json!({
            "status": "ok",
            "name": v["name"],
            "scope": v["scope"],
            "tags": v["tags"],
            "body_sha256": sha256_hex(v["content"].as_str().expect("read content")),
        }),
        ToolOutcome::Err { code, message } => error_record(code, message),
    }
}

fn qualified(scope: &str, name: &str) -> String {
    let scope: Scope = scope.parse().expect("scope in response");
    MemoryRef::new(scope, MemoryName::new(name).expect("name in response")).qualified_path()
}

/// Record one recall. `tags_all` is passed to the handler as-is; a
/// non-empty filter sends recall through the candidate allow-list.
///
/// Fused and BM25 scores come from a direct, unfiltered `hybrid_search`, so
/// they are recorded only for unfiltered recall: the allow-listed search is
/// crate-private, and an unfiltered fusion would rank, and so score, a
/// filtered hit differently. Filtered recall keeps everything the handler
/// reports (rank, distance, match type, counts).
async fn record_recall(
    world: &World,
    query: &str,
    scope: Option<&str>,
    tags_all: Option<&[String]>,
    limit: usize,
) -> Value {
    let tags_all = tags_all.filter(|tags| !tags.is_empty());
    let mut args = json!({ "query": query, "limit": limit });
    put(&mut args, "scope", scope);
    put(&mut args, "tags_all", tags_all);
    let v = match world.call("recall", args).await {
        ToolOutcome::Ok(v) => v,
        ToolOutcome::Err { code, message } => return error_record(code, message),
    };

    let filter = ScopeFilter::parse_or_default(scope).expect("scope filter");
    let allowed = match tags_all {
        Some(tags) => Some(tagged_names(world, tags).await),
        None => None,
    };
    assert_no_semantic_tie(world, &filter, allowed.as_ref(), query, limit);
    let scores: Option<BTreeMap<String, (f64, Option<f32>)>> = match allowed {
        Some(_) => None,
        None => Some(
            hybrid_search(
                world.state.embedding.as_ref(),
                world.state.index.as_ref(),
                &world.state.lexical,
                &filter,
                query,
                limit,
            )
            .await
            .expect("direct hybrid search")
            .into_iter()
            .map(|h| (h.qualified_name, (h.score, h.lexical_score)))
            .collect(),
        ),
    };

    let results = v["results"].as_array().expect("recall results");
    let hits: Vec<Value> = results
        .iter()
        .enumerate()
        .map(|(rank, r)| {
            let name = r["name"].as_str().expect("hit name");
            let scope = r["scope"].as_str().expect("hit scope");
            let mut hit = json!({
                "rank": rank,
                "name": name,
                "scope": scope,
                "match_type": r["match_type"],
                "distance": round(r["distance"].as_f64().expect("distance"), 4),
                "content_length": r["content_length"],
                "truncated": r["truncated"],
                "snippet_sha256": sha256_hex(r["content"].as_str().expect("snippet")),
            });
            if let Some(scores) = &scores {
                let (fused_score, lexical_score) = scores
                    .get(qualified(scope, name).as_str())
                    .copied()
                    .expect("handler hit must appear in the direct search");
                hit["fused_score"] = json!(round(fused_score, 6));
                hit["lexical_score"] = json!(lexical_score.map(|s| round(f64::from(s), 4)));
            }
            hit
        })
        .collect();
    let root_sentinel = hits.iter().any(|h| h["name"] == "root-sentinel-ottoline");
    json!({
        "status": "ok",
        "count": v["count"],
        "pre_filter_count": v["pre_filter_count"],
        "skipped_errors": v["skipped_errors"],
        "root_sentinel_returned": root_sentinel,
        "hits": hits,
    })
}

/// Qualified names of every memory carrying all of `tags`, from repository
/// truth: the set a `tags_all` recall may rank.
async fn tagged_names(world: &World, tags: &[String]) -> BTreeSet<String> {
    world
        .snapshot()
        .await
        .into_iter()
        .filter(|(_, (_, memory_tags))| tags.iter().all(|t| memory_tags.contains(t)))
        .map(|((scope, name), _)| qualified(&scope, &name))
        .collect()
}

/// Fail loudly if two semantic candidates inside the ranking window share a
/// distance: `InMemoryStore` orders such ties arbitrarily, which would make
/// the record flaky rather than wrong. The window is `limit` plus the first
/// candidate past the cut, since a tie there decides membership. With an
/// allow-list the window is over allowed candidates only, as recall ranks.
fn assert_no_semantic_tie(
    world: &World,
    filter: &ScopeFilter,
    allowed: Option<&BTreeSet<String>>,
    query: &str,
    limit: usize,
) {
    let index = world.state.index.as_ref();
    let mut candidates = index
        .search(
            filter,
            &StubEmbeddingBackend::embed_text(query),
            index.search_bound(filter),
        )
        .expect("direct semantic search");
    if let Some(allowed) = allowed {
        candidates.retain(|c| allowed.contains(&c.1));
    }
    for pair in candidates.windows(2).take(limit) {
        assert!(
            pair[0].2 != pair[1].2,
            "semantic tie at distance {} between {} and {} for query {query:?}; \
             adjust the query or corpus so the record is deterministic",
            pair[0].2,
            pair[0].1,
            pair[1].1
        );
    }
}

async fn record_list(world: &World, case: &Case) -> Value {
    let mut items = Vec::new();
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    let mut count;
    loop {
        let mut args = json!({
            "limit": LIST_PAGE_LIMIT,
            "fields": ["name", "scope", "tags", "content"],
        });
        put(&mut args, "scope", case.scope.clone());
        put(&mut args, "tags_all", case.tags_all.clone());
        put(&mut args, "cursor", cursor.clone());
        let v = match world.call("list", args).await {
            ToolOutcome::Ok(v) => v,
            ToolOutcome::Err { code, message } => return error_record(code, message),
        };
        count = v["count"].clone();
        let memories = v["memories"].as_array().expect("list memories");
        pages.push(memories.len());
        for m in memories {
            items.push(json!({
                "scope": m["scope"],
                "name": m["name"],
                "tags": m["tags"],
                "body_sha256": sha256_hex(m["content"].as_str().expect("list content")),
            }));
        }
        match v["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    let scopes: BTreeSet<&str> = items
        .iter()
        .map(|i| i["scope"].as_str().expect("scope"))
        .collect();
    let root_sentinel = items.iter().any(|i| i["name"] == "root-sentinel-ottoline");
    json!({
        "status": "ok",
        "count": count,
        "page_sizes": pages,
        "scopes_returned": scopes,
        "root_sentinel_returned": root_sentinel,
        "items": items,
    })
}

fn write_args(case: &Case) -> Value {
    let name = case.require(&case.name, "name");
    match case.op {
        Op::Remember => {
            let mut args = json!({
                "name": name,
                "content": case.require(&case.content, "content"),
                "tags": case.tags.clone().unwrap_or_default(),
            });
            put(&mut args, "scope", case.scope.clone());
            args
        }
        Op::Edit => {
            let mut args = json!({ "name": name });
            put(&mut args, "scope", case.scope.clone());
            put(&mut args, "content", case.content.clone());
            put(&mut args, "tags", case.tags.clone());
            args
        }
        Op::Move => {
            let mut args = json!({
                "name": name,
                "to_scope": case.require(&case.to_scope, "to_scope"),
            });
            put(&mut args, "from_scope", case.from_scope.clone());
            put(&mut args, "new_name", case.new_name.clone());
            args
        }
        Op::Forget => {
            let mut args = json!({ "name": name });
            put(&mut args, "scope", case.scope.clone());
            args
        }
        Op::Read | Op::Recall | Op::List => unreachable!("not a write: {:?}", case.op),
    }
}

fn state_change(before: &Snapshot, after: &Snapshot) -> Value {
    let entry = |(scope, name): &(String, String), (digest, tags): &(String, Vec<String>)| json!({ "scope": scope, "name": name, "body_sha256": digest, "tags": tags });
    let added: Vec<Value> = after
        .iter()
        .filter(|(k, _)| !before.contains_key(*k))
        .map(|(k, v)| entry(k, v))
        .collect();
    let removed: Vec<Value> = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .map(|(scope, name)| json!({ "scope": scope, "name": name }))
        .collect();
    let modified: Vec<Value> = after
        .iter()
        .filter(|(k, v)| before.get(*k).is_some_and(|b| b != *v))
        .map(|(k, v)| entry(k, v))
        .collect();
    json!({ "added": added, "removed": removed, "modified": modified })
}

async fn record_write(corpus: &[Memory], case: &Case) -> Value {
    // Each write runs against its own freshly loaded world so cases never
    // observe one another's effects.
    let world = build_world(corpus).await;
    let before = world.snapshot().await;
    let response = match world.call(case.op.tool(), write_args(case)).await {
        // Every response field is kept except the per-run ones, so a new field
        // (an overwrite warning on `remember`, say) fails replay.
        ToolOutcome::Ok(v) => match v {
            Value::Object(mut fields) => {
                for key in NEVER_RECORDED_RESPONSE_KEYS {
                    fields.remove(*key);
                }
                fields.insert("status".to_owned(), json!("ok"));
                Value::Object(fields)
            }
            other => json!({ "status": "ok", "text": other }),
        },
        ToolOutcome::Err { code, message } => error_record(code, message),
    };
    let after = world.snapshot().await;
    let mut record = json!({
        "response": response,
        "state_change": state_change(&before, &after),
    });
    if let Some(probe) = &case.probe {
        record["probe"] = json!({
            "query": probe.query,
            "scope": probe.scope,
            "result": record_recall(
                &world,
                &probe.query,
                probe.scope.as_deref(),
                None,
                probe.limit.unwrap_or(DEFAULT_RECALL_LIMIT),
            )
            .await,
        });
    }
    record
}

async fn record_case(corpus: &[Memory], shared: &World, case: &Case) -> Value {
    let before = match case.op {
        Op::Read => record_read(shared, case).await,
        Op::Recall => {
            record_recall(
                shared,
                case.require(&case.query, "query"),
                case.scope.as_deref(),
                case.tags_all.as_deref(),
                case.limit.unwrap_or(DEFAULT_RECALL_LIMIT),
            )
            .await
        }
        Op::List => record_list(shared, case).await,
        Op::Remember | Op::Edit | Op::Move | Op::Forget => record_write(corpus, case).await,
    };
    // A projection naming an absent field would compare nothing and pass
    // vacuously, so every observed path must exist in the observed record.
    for (path, raw) in case
        .observe_paths()
        .unwrap_or_default()
        .iter()
        .zip(case.observe.iter().flatten())
    {
        assert!(
            path_resolves(&before, path),
            "case {}: observe field `{raw}` is not in the observed record:\n{}",
            case.id,
            pretty(&before)
        );
    }
    json!({
        "id": case.id,
        "op": case.op,
        "view": case.view,
        "principal": case.principal,
        "before": before,
    })
}

/// The last commit that changed pali's code (`src/`, `Cargo.toml`,
/// `Cargo.lock`) when the record was written. Commits that touch only this
/// fixture leave it unchanged.
///
/// This is a label, not a pin: replay never compares it, because most code
/// commits leave access behaviour alone and comparing would fail the fixture
/// on every one of them. Replay compares behaviour; the label only says, in
/// the failure text, which code the record was taken from.
fn recorded_code_revision() -> String {
    std::process::Command::new("git")
        .args([
            "log",
            "-1",
            "--format=%H",
            "--",
            "src",
            "Cargo.toml",
            "Cargo.lock",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Observation projection
// ---------------------------------------------------------------------------

/// Whether `path` names a field present in `value`. Arrays are transparent:
/// a path resolves through an array when it resolves in every element.
fn path_resolves(value: &Value, path: &[&str]) -> bool {
    match (path.split_first(), value) {
        (None, _) => true,
        (Some(_), Value::Array(items)) => items.iter().all(|item| path_resolves(item, path)),
        (Some((head, rest)), Value::Object(map)) => {
            map.get(*head).is_some_and(|v| path_resolves(v, rest))
        }
        (Some(_), _) => false,
    }
}

/// Keep only the fields named by `paths` (dotted, arrays transparent).
fn project(value: &Value, paths: &[Vec<&str>]) -> Value {
    if paths.iter().any(|p| p.is_empty()) {
        return value.clone();
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|i| project(i, paths)).collect()),
        Value::Object(map) => {
            let mut grouped: BTreeMap<&str, Vec<Vec<&str>>> = BTreeMap::new();
            for path in paths {
                grouped.entry(path[0]).or_default().push(path[1..].to_vec());
            }
            let projected = grouped
                .into_iter()
                .filter_map(|(key, rest)| {
                    map.get(key).map(|v| (key.to_string(), project(v, &rest)))
                })
                .collect();
            Value::Object(projected)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Replay diff
// ---------------------------------------------------------------------------

/// Line diff of two texts via longest common subsequence; `-` lines are in
/// the record only, `+` lines in the observed behaviour only.
fn line_diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let mut lcs = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = String::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
            out.push_str(&format!("+ {}\n", b[j]));
            j += 1;
        } else {
            out.push_str(&format!("- {}\n", a[i]));
            i += 1;
        }
    }
    out
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("serialise JSON")
}

/// Compare `section` entries keyed by `key`, reporting each difference.
fn diff_keyed(section: &str, key: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    let index = |v: &Value| -> BTreeMap<String, Value> {
        v.as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let k = match key {
                            "corpus" => format!("{}/{}", item["scope"], item["name"]),
                            _ => item[key].as_str().unwrap_or_default().to_string(),
                        };
                        (k, item.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let (exp, act) = (index(expected), index(actual));
    for (k, e) in &exp {
        match act.get(k) {
            None => out.push(format!("{section} {k}: in the record, not observed")),
            Some(a) if a != e => out.push(format!(
                "{section} {k}:\n{}",
                line_diff(&pretty(e), &pretty(a))
            )),
            Some(_) => {}
        }
    }
    for k in act.keys().filter(|k| !exp.contains_key(*k)) {
        out.push(format!("{section} {k}: observed, not in the record"));
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_fixture_before_matches_record() {
    let corpus = common::load_access_corpus();
    let corpus_scopes: BTreeSet<String> = corpus
        .iter()
        .map(|m| m.metadata.scope.to_string())
        .collect();
    let cases = load_cases(&corpus_scopes);

    let shared = build_world(&corpus).await;
    let mut observed_cases = Vec::new();
    for case in &cases {
        observed_cases.push(record_case(&corpus, &shared, case).await);
    }
    let observed_corpus = corpus_record(&corpus);

    let record_path = fixture_dir().join("before.json");
    if std::env::var_os(RECORD_ENV).is_some_and(|v| v == "1") {
        let record = json!({
            "format": RECORD_FORMAT,
            "pali_version": env!("CARGO_PKG_VERSION"),
            "recorded_code_revision_label": recorded_code_revision(),
            "embedding": format!("fnv1a-hashed-words-and-trigrams/{DIMS}"),
            "regenerate": format!("{RECORD_ENV}=1 cargo test --test access_fixture"),
            "never_recorded": NEVER_RECORDED,
            "corpus": observed_corpus,
            "cases": observed_cases,
        });
        std::fs::write(&record_path, pretty(&record) + "\n").expect("write before.json");
        eprintln!("access fixture: recorded {} cases", cases.len());
        return;
    }

    let raw = std::fs::read_to_string(&record_path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}; run with {RECORD_ENV}=1 to create it",
            record_path.display()
        )
    });
    let record: Value = serde_json::from_str(&raw).expect("parse before.json");
    assert_eq!(
        record["format"].as_u64(),
        Some(RECORD_FORMAT),
        "before.json format version"
    );

    // Compare each case under its observation projection; the record itself
    // always holds the full observation.
    let projections: BTreeMap<&str, Vec<Vec<&str>>> = cases
        .iter()
        .filter_map(|c| c.observe_paths().map(|p| (c.id.as_str(), p)))
        .collect();
    let apply = |cases: &Value| -> Value {
        Value::Array(
            cases
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| {
                            let id = item["id"].as_str().unwrap_or_default();
                            match projections.get(id) {
                                Some(paths) => {
                                    let mut item = item.clone();
                                    item["before"] = project(&item["before"], paths);
                                    item
                                }
                                None => item.clone(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    };

    let mut differences = Vec::new();
    diff_keyed(
        "corpus",
        "corpus",
        &record["corpus"],
        &observed_corpus,
        &mut differences,
    );
    diff_keyed(
        "case",
        "id",
        &apply(&record["cases"]),
        &apply(&Value::Array(observed_cases)),
        &mut differences,
    );
    assert!(
        differences.is_empty(),
        "access fixture differs from before.json (recorded at pali {}, code revision \
         label {}, not checked); {} difference(s). If the change is intended, rerun \
         with {RECORD_ENV}=1.\n\n{}",
        record["pali_version"],
        record["recorded_code_revision_label"],
        differences.len(),
        differences.join("\n")
    );
}
