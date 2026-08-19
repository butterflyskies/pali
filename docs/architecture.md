# Architecture

Pali is a single Rust binary that serves Model Context Protocol over
Streamable HTTP. Its durable state is a git repository of Markdown files;
retrieval indexes and recall telemetry are local derived state.

```text
MCP client
    │ Streamable HTTP
    ▼
Axum + rmcp server
    ├── MemoryRepo ──▶ Markdown + YAML frontmatter ──▶ git remote
    ├── CandleEmbeddingEngine ──▶ scope-partitioned usearch indexes
    ├── LexicalIndex ──▶ in-memory Tantivy BM25 index
    └── RecallLog ──▶ local SQLite telemetry
```

## Durable source of truth

Each memory is a Markdown file with YAML frontmatter containing its stable ID,
name, scope, tags, source, and timestamps. Repository operations stage and
commit changes through libgit2. The files remain readable and editable without
Pali, and git preserves their history.

The remote is optional. `sync` pulls, resolves conflicts by `updated_at`, and
pushes the configured branch. Authentication is acquired only when remote work
needs it.

## Retrieval

Two derived indexes answer different questions:

- Candle embeds memory content with BGE-small-en-v1.5. usearch stores the
  vectors in scope-partitioned HNSW indexes for conceptual similarity.
- Tantivy indexes names and content in memory for BM25 term and exact-phrase
  retrieval.

`recall` runs both strategies and combines ranks with reciprocal rank fusion.
The response records whether each result came from semantic search, lexical
search, or both. Search results carry snippets; callers use `read` for complete
content.

The Markdown repository remains authoritative. Indexes are rebuilt or updated
from repository state rather than becoming another memory store.

The lexical index is rebuilt from the repository at startup, and later git
mutations are mirrored into it. Those mirrors follow a complete-or-degraded
contract. A failed or interrupted mirror is never served as current: recall
temporarily falls back to semantic-only results while a single-flight
background repair rebuilds the lexical index from git truth. Repair is also
retried after startup rebuild failures and when recall observes degradation.
Lexical degradation does not gate readiness.

## Scopes

`global` is the root namespace. Bare paths such as `project` and
`org/team/project` create hierarchical namespaces. Scope selection determines
which indexes and repository subtrees a query searches. It does not decide who
is authorized to read a memory.

## Concurrency and blocking work

The asynchronous HTTP server keeps blocking components behind explicit seams:

- a dedicated worker thread owns the Candle model and serves a bounded queue;
- libgit2 and SQLite operations run through blocking boundaries;
- lexical mutations are batched and committed on Tokio's blocking pool;
- passive subsystem reporters make readiness checks avoid repository or model
  I/O.

MCP sessions are bounded by count, rate, idle timeout, and optional maximum
lifetime.

## Telemetry

Structured tracing covers tool handlers and subsystem operations. Successful
tool responses include server processing duration metadata.

Recall events are written to local SQLite. `mark_applied` and
`batch_mark_applied` attach usefulness verdicts to a recall, and `recall_stats`
aggregates them by semantic distance. Telemetry is local derived state and does
not sync with the Markdown repository.

## Decisions and change control

The [architecture decision records](adr/) are binding. They document transport,
storage, indexes, authentication, health reporting, metadata, and retrieval
trade-offs. New architecture changes should update or supersede those records
rather than silently changing this overview.
