//! Domain types: memories, scopes, metadata, validation, and application state.

mod args;
mod chunk;
mod memory;
mod scope;
mod validated;

// Re-export everything that was previously public from types.rs.
// Items that were `pub` remain `pub`; items that were `pub(crate)` are
// re-exported as `pub(crate)`.

pub(crate) use args::ResolvedChanges;
pub use args::{
    AppState, BatchMarkAppliedArgs, ChangedMemories, EditArgs, ForgetArgs, ListArgs, ListField,
    MarkAppliedArgs, MoveArgs, PullResult, ReadArgs, RecallArgs, RecallStatsArgs, ReindexStats,
    RememberArgs, SyncArgs, Verdict, VerdictEntry,
};

pub(crate) use args::{ListToolArgs, ListToolField, ReadToolArgs, RecallToolArgs, LIST_MAX_LIMIT};
pub(crate) use scope::scope_path_matches;

pub use chunk::{ChunkerVersion, FactId, SourceSpan};

// The catalog and recall-wire shapes (`FactRecord`, `MatchedChunk`) are
// deliberately not re-exported: they stay internal to `chunk` until
// their owning slices (#262 slices 3 and 7) produce a consumer
// (ADR-0042) — every `pub` item is a semver commitment.

pub use memory::{parse_qualified_name, Memory, MemoryMetadata, MemoryName, MemoryRef};

pub use scope::{validate_branch_name, Scope, ScopeFilter, ScopePath};
