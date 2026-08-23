use std::{fmt, sync::Arc};

use rmcp::schemars;
use serde::Deserialize;

use crate::{
    auth::AuthProvider, embedding::EmbeddingBackend, health::HealthRegistry, index::VectorStore,
    repo::MemoryRepo, repo_router::RepoRouter,
};

pub(crate) const LIST_MAX_LIMIT: usize = 100;

// ---------------------------------------------------------------------------
// Tool argument structs
// ---------------------------------------------------------------------------

/// Arguments for the `remember` tool — store a new memory.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// The content to store. Markdown is supported.
    pub content: String,
    /// Human-readable name for this memory (used as the filename stem).
    pub name: String,
    /// Optional list of tags for categorisation.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Scope: 'global' or a bare namespace path like 'my-project' or 'org/team'. Defaults to 'global'.
    #[serde(default)]
    pub scope: Option<String>,
    /// Optional hint about the source of this memory.
    #[serde(default)]
    pub source: Option<String>,
}

/// Arguments for the `recall` tool — semantic search.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Natural-language query to search for.
    pub query: String,
    /// Scope: 'global', a bare namespace path like 'my-project' or 'org/team', 'all', or omit for global-only. Use the basename of your cwd (or its path) to search your current namespace + global memories. Use 'all' to search across every scope.
    #[serde(default)]
    pub scope: Option<String>,
    /// Maximum number of results to return. Defaults to 5.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Arguments for the `forget` tool — delete a memory.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// Exact name of the memory to delete.
    pub name: String,
    /// Scope of the memory. Defaults to 'global'. Use a bare namespace path like 'my-project' or 'org/team' for scoped memories.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Arguments for the `edit` tool — modify an existing memory.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditArgs {
    /// Name of the memory to edit.
    pub name: String,
    /// New content (replaces existing). Omit to keep current content.
    #[serde(default)]
    pub content: Option<String>,
    /// New tag list (replaces existing). Omit to keep current tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Scope of the memory. Defaults to 'global'. Use a bare namespace path like 'my-project' or 'org/team' for scoped memories.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Arguments for the `move` tool — relocate a memory between scopes.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    /// Exact name of the memory to move.
    pub name: String,
    /// Source scope. Defaults to 'global'. Use a bare namespace path like 'my-project' or 'org/team' for scoped memories.
    #[serde(default)]
    pub from_scope: Option<String>,
    /// Destination scope. Required. Use a bare namespace path like 'my-project' or 'org/team', or 'global'.
    pub to_scope: String,
    /// Optional new name for the memory in the destination scope. Defaults to the original name.
    #[serde(default)]
    pub new_name: Option<String>,
}

/// A summary field that can be returned by the `list` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ListField {
    /// Stable memory UUID.
    Id,
    /// Human-readable memory name.
    Name,
    /// Fully-qualified memory scope.
    Scope,
    /// Free-form memory tags.
    Tags,
    /// Memory creation timestamp.
    CreatedAt,
    /// Most recent memory update timestamp.
    UpdatedAt,
}

impl ListField {
    /// The compatibility projection used when callers omit `fields`.
    pub(crate) const ALL: [Self; 6] = [
        Self::Id,
        Self::Name,
        Self::Scope,
        Self::Tags,
        Self::CreatedAt,
        Self::UpdatedAt,
    ];
}

/// Legacy public Rust DTO for the original, scope-only `list` request.
///
/// The MCP handler now uses a crate-private wire type so its additive JSON
/// fields do not silently break downstream code that constructs this public
/// struct. This type remains public for semver compatibility even though the
/// handler itself is not part of the public Rust API.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Scope: 'global', a bare namespace path like 'my-project' or 'org/team', 'all', or omit for global-only. Use 'all' to list everything.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Wire arguments for the paginated `list` MCP tool.
///
/// Kept crate-private so extending the tool's JSON request does not break
/// downstream Rust callers that construct the legacy public [`ListArgs`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListToolArgs {
    /// Scope: 'global', a bare namespace path like 'my-project' or 'org/team', 'all', or omit for global-only. Use 'all' to list everything.
    #[serde(default)]
    pub scope: Option<String>,
    /// Maximum number of summaries to return. Defaults to 50; maximum 100.
    #[serde(default)]
    #[schemars(range(min = 1, max = LIST_MAX_LIMIT))]
    pub limit: Option<usize>,
    /// Opaque cursor returned by a previous list page. Cursors are bound to the queried scope.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Summary fields to return. Omit to preserve the full six-field legacy summary.
    #[serde(default)]
    pub fields: Option<Vec<ListField>>,
}

/// Arguments for the `read` tool — retrieve a specific memory by name.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Exact name of the memory to read.
    pub name: String,
    /// Scope of the memory. Defaults to 'global'. Use a bare namespace path like 'my-project' or 'org/team' for scoped memories.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Wire arguments for the additive federated `read` MCP mode.
///
/// Kept crate-private so adding the optional JSON field does not break Rust
/// callers that construct the legacy public [`ReadArgs`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReadToolArgs {
    /// Exact name of the memory to read.
    pub name: String,
    /// Scope of the memory. Defaults to `global`.
    #[serde(default)]
    pub scope: Option<String>,
    /// Read the same name from every configured sibling store.
    #[serde(default)]
    pub straddle: bool,
}

/// Agent's assessment of whether a recalled memory was useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Memory materially influenced the session.
    Applied,
    /// Memory was partially relevant or influence was uncertain.
    Maybe,
    /// Memory was not relevant to the session.
    NotApplied,
}

impl Verdict {
    /// String representation for SQLite storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Applied => "applied",
            Verdict::Maybe => "maybe",
            Verdict::NotApplied => "not_applied",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Arguments for the `mark_applied` tool — report memory usage back to the recall log.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkAppliedArgs {
    /// The recall_id from the recall response that returned this memory.
    pub recall_id: String,
    /// Name of the memory that was (or was not) applied.
    pub memory: String,
    /// Agent's assessment of whether the memory was useful: 'applied', 'maybe', or 'not_applied'.
    pub verdict: Verdict,
    /// Brief description of how the memory influenced the session.
    #[serde(default)]
    pub application: Option<String>,
    /// Confidence level: "high", "medium", or "low".
    #[serde(default = "default_confidence")]
    pub confidence: String,
}

fn default_confidence() -> String {
    "medium".to_string()
}

/// A single verdict entry within a [`BatchMarkAppliedArgs`] request.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct VerdictEntry {
    /// The recall_id from the recall response that returned this memory.
    pub recall_id: String,
    /// Name of the memory that was (or was not) applied.
    pub memory: String,
    /// Agent's assessment of whether the memory was useful: 'applied', 'maybe', or 'not_applied'.
    pub verdict: Verdict,
    /// Brief description of how the memory influenced the session.
    #[serde(default)]
    pub application: Option<String>,
    /// Confidence level: "high", "medium", or "low".
    #[serde(default = "default_confidence")]
    pub confidence: String,
}

/// Arguments for the `batch_mark_applied` tool — report multiple memory verdicts in a single call.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchMarkAppliedArgs {
    /// Array of verdict entries, each containing a recall_id, memory name, verdict, and optional metadata.
    pub verdicts: Vec<VerdictEntry>,
}

/// Arguments for the `sync` tool — push/pull the git remote.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SyncArgs {
    /// If true, pull before pushing. Defaults to true.
    #[serde(default)]
    pub pull_first: Option<bool>,
}

// ---------------------------------------------------------------------------
// PullResult
/// Arguments for the `recall_stats` tool — no parameters required.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallStatsArgs {}

// ---------------------------------------------------------------------------

/// The outcome of a `pull()` operation.
#[derive(Debug)]
#[non_exhaustive]
pub enum PullResult {
    /// No `origin` remote is configured — running in local-only mode.
    NoRemote,
    /// The local branch was already up to date with the remote.
    UpToDate,
    /// The remote was ahead and the branch was fast-forwarded.
    FastForward {
        /// Commit OID before the fast-forward.
        old_head: [u8; 20],
        /// Commit OID after the fast-forward.
        new_head: [u8; 20],
    },
    /// A merge was performed; `conflicts_resolved` counts auto-resolved files.
    Merged {
        /// Number of conflicting files that were auto-resolved.
        conflicts_resolved: usize,
        /// Commit OID before the merge.
        old_head: [u8; 20],
        /// Commit OID after the merge.
        new_head: [u8; 20],
    },
}

// ---------------------------------------------------------------------------
// ChangedMemories
// ---------------------------------------------------------------------------

/// Memories that changed between two git commits.
///
/// This is the crate's published (0.16.0) change-set surface: repository-path
/// strings (without the `.md` suffix, e.g. `"global/foo"`, `"projects/a/b/mem"`)
/// for added/modified and deleted memories, derived straight from the git
/// deltas. It is produced by
/// [`crate::repo::MemoryRepo::diff_changed_memories`], which reports every
/// changed `.md` file — including ones whose blob is non-UTF-8 or has
/// unparseable frontmatter — because a path is available even when the content
/// cannot be resolved to a memory.
///
/// Internally the index-mirror path does NOT consume these strings: it uses the
/// richer crate-internal `ResolvedChanges` (structured [`super::MemoryRef`]s
/// plus an `unresolved` count, produced by
/// `MemoryRepo::diff_changed_refs`) so hierarchical scopes are resolved from
/// frontmatter rather than by splitting a path back into scope + name, which is
/// ambiguous (`projects/a/b/mem.md` could be scope `a/b`, name `mem` or scope
/// `a`, name `b/mem`).
#[derive(Debug, Default)]
pub struct ChangedMemories {
    /// Repository-path qualified names without the `.md` suffix (e.g.
    /// `"global/foo"`) that were added or modified.
    pub upserted: Vec<String>,
    /// Repository-path qualified names (e.g. `"global/foo"`) that were deleted.
    pub removed: Vec<String>,
}

impl ChangedMemories {
    /// Returns `true` if there are no changes.
    pub fn is_empty(&self) -> bool {
        self.upserted.is_empty() && self.removed.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ResolvedChanges (crate-internal)
// ---------------------------------------------------------------------------

/// Changed memories resolved to structured references, for the index mirror.
///
/// Every changed file is resolved to a [`super::MemoryRef`] by parsing the
/// blob's YAML frontmatter (the new tree for upserts, the old tree for
/// removals) — the same authority `list_memories` uses to build the canonical
/// index keys, so hierarchical scope paths are never split ambiguously
/// (`projects/a/b/mem.md` is unambiguously scope `a/b`, name `mem`, not scope
/// `a`, name `b/mem`). Files that cannot be resolved (unparseable frontmatter,
/// non-UTF-8 content, or a non-memory object such as a symlink) are counted in
/// `unresolved` so callers degrade derived indexes instead of committing a
/// silently reduced change set.
///
/// Crate-internal on purpose: it carries the structured `scope`/`name` the
/// mirror needs, and keeping it private preserves the published
/// [`ChangedMemories`] API without a version bump.
#[derive(Debug, Default)]
pub(crate) struct ResolvedChanges {
    /// Memories that were added or modified (resolved from the new tree).
    pub(crate) upserted: Vec<super::MemoryRef>,
    /// Memories that were deleted (resolved from the old tree).
    pub(crate) removed: Vec<super::MemoryRef>,
    /// Changed memory files whose reference could not be resolved. Any
    /// non-zero value means the change set is incomplete: derived indexes
    /// must be marked for rebuild rather than treated as fully mirrored.
    pub(crate) unresolved: usize,
}

impl ResolvedChanges {
    /// Returns `true` if there are no resolved changes to apply.
    ///
    /// Deliberately ignores `unresolved` — callers must check that field
    /// separately, because an all-unresolved change set still requires the
    /// derived indexes to be degraded and repaired.
    pub(crate) fn is_empty(&self) -> bool {
        self.upserted.is_empty() && self.removed.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ReindexStats
// ---------------------------------------------------------------------------

/// Statistics from an incremental reindex operation.
#[derive(Debug, Default)]
pub struct ReindexStats {
    /// Number of newly indexed memories.
    pub added: usize,
    /// Number of memories whose embeddings were refreshed.
    pub updated: usize,
    /// Number of memories removed from the index.
    pub removed: usize,
    /// Number of memories that failed to index.
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Shared application state threaded through the Axum server.
///
/// Wrapped in a single outer `Arc` at the call site. `repo` is additionally
/// wrapped in its own `Arc` so it can be cloned into `spawn_blocking` closures.
#[non_exhaustive]
pub struct AppState {
    /// Git-backed memory repository (default repo, for backward compat).
    pub repo: Arc<MemoryRepo>,
    /// Routes memory operations to scope-specific repos when configured.
    pub router: RepoRouter,
    /// Backend used to compute text embeddings.
    pub embedding: Box<dyn EmbeddingBackend>,
    /// In-memory vector index for semantic search (scope-partitioned).
    pub index: Box<dyn VectorStore>,
    /// In-RAM BM25 index for keyword search (rebuilt from the repo on startup).
    pub lexical: Arc<crate::search::LexicalIndex>,
    /// Authentication provider for API access control.
    pub auth: AuthProvider,
    /// Branch name used for push/pull operations (default: "main").
    pub branch: String,
    /// Passive health registry — subsystems report here, `/readyz` reads here.
    pub health: HealthRegistry,
    /// Optional append-only recall event log for threshold calibration.
    pub recall_log: Option<Arc<crate::recall_log::RecallLog>>,
    /// `true` while every git HEAD advance since the vector index's stored
    /// commit SHA has been fully mirrored into the vector index.
    ///
    /// Cleared — sticky for the process lifetime — when a mirror gap opens
    /// that only a full reindex can close: post-pull change discovery fails,
    /// pulled files cannot be resolved, an incremental reindex records any
    /// item-level error, or the startup reindex did not complete cleanly
    /// (#293 review, round 3: partial success is not certification). While
    /// cleared, composite-SHA advancement is refused at both sync and
    /// shutdown, so the next startup detects the SHA mismatch and rebuilds
    /// the vector index from git truth.
    index_mirror_intact: std::sync::atomic::AtomicBool,
}

impl AppState {
    /// Create a new application state from subsystem instances.
    pub fn new(
        repo: Arc<MemoryRepo>,
        branch: String,
        embedding: Box<dyn EmbeddingBackend>,
        index: Box<dyn VectorStore>,
        auth: AuthProvider,
        health: HealthRegistry,
        recall_log: Option<Arc<crate::recall_log::RecallLog>>,
    ) -> Self {
        let router = RepoRouter::single(Arc::clone(&repo));
        Self {
            repo,
            router,
            embedding,
            index,
            lexical: Arc::new(crate::search::LexicalIndex::new()),
            auth,
            branch,
            health,
            recall_log,
            index_mirror_intact: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Create a new application state with a pre-built repo router.
    #[allow(clippy::too_many_arguments)]
    pub fn with_router(
        repo: Arc<MemoryRepo>,
        router: RepoRouter,
        branch: String,
        embedding: Box<dyn EmbeddingBackend>,
        index: Box<dyn VectorStore>,
        auth: AuthProvider,
        health: HealthRegistry,
        recall_log: Option<Arc<crate::recall_log::RecallLog>>,
    ) -> Self {
        Self {
            repo,
            router,
            embedding,
            index,
            // Constructed here rather than injected: creation is infallible
            // (a failure yields a disabled instance) and the index starts
            // empty either way — callers populate it via
            // `search::rebuild_lexical_from_repo` or the write handlers.
            lexical: Arc::new(crate::search::LexicalIndex::new()),
            auth,
            branch,
            health,
            recall_log,
            index_mirror_intact: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Record that the vector index has a mirror gap only a full reindex can
    /// close, blocking composite-SHA advancement for the rest of the process
    /// lifetime (see [`AppState::index_mirror_is_intact`]).
    pub fn mark_index_mirror_incomplete(&self) {
        self.index_mirror_intact
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// `true` when the vector index mirror has no known gap and composite-SHA
    /// advancement is allowed.
    pub fn index_mirror_is_intact(&self) -> bool {
        self.index_mirror_intact
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Wire-contract tests — assert the JSON schema shape that MCP clients see.
    // -----------------------------------------------------------------------

    #[test]
    fn batch_mark_applied_schema_has_verdicts_array() {
        let schema = schemars::schema_for!(BatchMarkAppliedArgs);
        let root = serde_json::to_value(&schema).unwrap();

        let props = root["properties"].as_object().unwrap();
        assert!(
            props.contains_key("verdicts"),
            "schema must expose a 'verdicts' property"
        );

        let required = root["required"].as_array().unwrap();
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            required_strs.contains(&"verdicts"),
            "'verdicts' must be required"
        );
    }

    #[test]
    fn verdict_entry_schema_has_required_fields() {
        let schema = schemars::schema_for!(VerdictEntry);
        let root = serde_json::to_value(&schema).unwrap();

        let props = root["properties"].as_object().unwrap();
        for field in &["recall_id", "memory", "verdict"] {
            assert!(
                props.contains_key(*field),
                "VerdictEntry schema must contain '{field}'"
            );
        }

        let required = root["required"].as_array().unwrap();
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        for field in &["recall_id", "memory", "verdict"] {
            assert!(
                required_strs.contains(field),
                "'{field}' must be required in VerdictEntry"
            );
        }
    }

    #[test]
    fn verdict_entry_optional_fields_not_required() {
        let schema = schemars::schema_for!(VerdictEntry);
        let root = serde_json::to_value(&schema).unwrap();

        let required = root["required"].as_array().unwrap();
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !required_strs.contains(&"application"),
            "'application' must not be required"
        );
    }

    // -----------------------------------------------------------------------
    // Deserialization round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_tool_args_deserializes_minimal() {
        let args: ListToolArgs = serde_json::from_str(r#"{"scope":"all"}"#).unwrap();
        assert_eq!(args.scope.as_deref(), Some("all"));
        assert_eq!(args.limit, None);
        assert_eq!(args.cursor, None);
        assert_eq!(args.fields, None);
    }

    #[test]
    fn list_tool_args_deserializes_full() {
        let args: ListToolArgs = serde_json::from_str(
            r#"{"scope":"all","limit":25,"cursor":"lc1_abc","fields":["name","scope"]}"#,
        )
        .unwrap();
        assert_eq!(args.scope.as_deref(), Some("all"));
        assert_eq!(args.limit, Some(25));
        assert_eq!(args.cursor.as_deref(), Some("lc1_abc"));
        assert_eq!(args.fields, Some(vec![ListField::Name, ListField::Scope]));
    }

    #[test]
    fn list_tool_args_rejects_unknown_field_variant() {
        let error = serde_json::from_str::<ListToolArgs>(r#"{"fields":["content"]}"#)
            .expect_err("unknown projection field must fail");
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn read_tool_args_preserves_legacy_defaults() {
        let args: ReadToolArgs = serde_json::from_str(r#"{"name":"person-cammy"}"#).unwrap();
        assert_eq!(args.name, "person-cammy");
        assert_eq!(args.scope, None);
        assert!(!args.straddle);
    }

    #[test]
    fn read_tool_args_accepts_straddle() {
        let args: ReadToolArgs =
            serde_json::from_str(r#"{"name":"person-cammy","scope":"global","straddle":true}"#)
                .unwrap();
        assert_eq!(args.scope.as_deref(), Some("global"));
        assert!(args.straddle);
    }

    #[test]
    fn batch_mark_applied_deserializes_minimal() {
        let json = r#"{
            "verdicts": [
                { "recall_id": "r_abc", "memory": "foo", "verdict": "applied" }
            ]
        }"#;
        let args: BatchMarkAppliedArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.verdicts.len(), 1);
        assert_eq!(args.verdicts[0].recall_id, "r_abc");
        assert_eq!(args.verdicts[0].memory, "foo");
        assert_eq!(args.verdicts[0].verdict, Verdict::Applied);
        assert_eq!(args.verdicts[0].confidence, "medium");
        assert!(args.verdicts[0].application.is_none());
    }

    #[test]
    fn batch_mark_applied_deserializes_full() {
        let json = r#"{
            "verdicts": [
                {
                    "recall_id": "r_1",
                    "memory": "m1",
                    "verdict": "applied",
                    "application": "used for greeting",
                    "confidence": "high"
                },
                {
                    "recall_id": "r_2",
                    "memory": "m2",
                    "verdict": "not_applied",
                    "confidence": "low"
                },
                {
                    "recall_id": "r_3",
                    "memory": "m3",
                    "verdict": "maybe"
                }
            ]
        }"#;
        let args: BatchMarkAppliedArgs = serde_json::from_str(json).unwrap();
        assert_eq!(args.verdicts.len(), 3);
        assert_eq!(args.verdicts[0].verdict, Verdict::Applied);
        assert_eq!(args.verdicts[0].confidence, "high");
        assert_eq!(
            args.verdicts[0].application.as_deref(),
            Some("used for greeting")
        );
        assert_eq!(args.verdicts[1].verdict, Verdict::NotApplied);
        assert_eq!(args.verdicts[1].confidence, "low");
        assert_eq!(args.verdicts[2].verdict, Verdict::Maybe);
        assert_eq!(args.verdicts[2].confidence, "medium");
    }

    #[test]
    fn batch_mark_applied_rejects_empty_json() {
        let json = r#"{}"#;
        let result: Result<BatchMarkAppliedArgs, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing 'verdicts' should fail");
    }

    #[test]
    fn verdict_entry_rejects_invalid_verdict() {
        let json = r#"{ "recall_id": "r_1", "memory": "m1", "verdict": "bogus" }"#;
        let result: Result<VerdictEntry, _> = serde_json::from_str(json);
        assert!(result.is_err(), "invalid verdict variant should fail");
    }

    #[test]
    fn list_schema_exposes_pagination_and_projection_inputs() {
        let schema = schemars::schema_for!(ListToolArgs);
        let root = serde_json::to_value(&schema).unwrap();
        let props = root["properties"].as_object().unwrap();

        for field in ["scope", "limit", "cursor", "fields"] {
            assert!(
                props.contains_key(field),
                "list schema must expose '{field}'"
            );
        }

        let limit_schema = serde_json::to_string(&props["limit"]).unwrap();
        assert!(limit_schema.contains("\"minimum\":1"), "{limit_schema}");
        assert!(limit_schema.contains("\"maximum\":100"), "{limit_schema}");

        let serialized = serde_json::to_string(&props["fields"]).unwrap();
        assert!(
            !serialized.contains("$ref"),
            "fields schema must be inline for MCP clients: {serialized}"
        );
        for field in ["id", "name", "scope", "tags", "created_at", "updated_at"] {
            assert!(
                serialized.contains(&format!("\"{field}\"")),
                "fields schema must advertise '{field}': {serialized}"
            );
        }
    }

    #[test]
    fn read_schema_exposes_optional_straddle_input() {
        let schema = schemars::schema_for!(ReadToolArgs);
        let root = serde_json::to_value(&schema).unwrap();
        let props = root["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("scope"));
        assert!(props.contains_key("straddle"));
        let required = root["required"].as_array().unwrap();
        assert_eq!(required, &[serde_json::json!("name")]);
    }
}
