# MCP tool reference

Pali exposes the following tools. Tool schemas returned during MCP discovery are
the authoritative machine-readable contract; this page explains how the tools
fit together.

## Memory lifecycle

| Tool | Purpose |
|---|---|
| `remember` | Store content, tags, source, and an optional scope; commit it to git and index it. |
| `read` | Fetch one memory's complete content and metadata by name and scope. |
| `edit` | Replace content or tags while preserving omitted fields. |
| `move` | Atomically move a memory to another scope, optionally renaming it. |
| `forget` | Delete a memory from git and the search indexes. |
| `list` | List memory summaries without full content. |

Memory names may contain up to three path components. Names and scopes are
validated before they become filesystem paths.

### `list`

`list` returns a bounded page of summaries sorted by scope and name. `limit`
defaults to 50 and accepts values from 1 through 100. When `has_more` is true,
pass the opaque `next_cursor` into the next request with the same scope. The
response distinguishes `count` (all matching memories) from `returned` (this
page). Cursors use keyset semantics, so concurrent inserts or deletes can change
later pages without invalidating the cursor.

Use `fields` to request an exact summary projection. Omitting it returns `id`,
`name`, `scope`, `tags`, `created_at`, and `updated_at`. Each successful page is
capped at 24 KiB; request fewer fields if a summary is too large.

## Retrieval

### `recall`

`recall` accepts a natural-language `query`, an optional `scope`, and an
optional `limit` (default 5).

It runs two independent retrieval strategies:

1. local embeddings search the scope-partitioned HNSW vector indexes;
2. Tantivy searches the in-memory BM25 lexical index, with exact phrases ranked
   ahead of term-only matches.

The ranked lists are merged with reciprocal rank fusion. A result includes:

- `name`, `scope`, and `tags`;
- a content snippet of at most 500 characters;
- `truncated` and `content_length`, so a caller knows when to use `read`;
- `match_type`: `semantic`, `lexical`, or `both`;
- `distance`: cosine distance for semantic hits, or `-1.0` for lexical-only
  hits;
- a batch-level `recall_id` used by the feedback tools.

Lower non-negative distances are more similar. Do not interpret `-1.0` as a
high-confidence semantic match; it means the result had no embedding distance.

If a lexical-index update fails or is interrupted, Pali marks that
derived index degraded rather than serving stale keyword results. Recall
continues with semantic-only results while a single-flight background repair
rebuilds the lexical index from the git-backed source of truth.

## Scopes

Scopes are hierarchical namespace paths:

- omit scope or pass `global` to query global memories only;
- pass `my-project` to query that subtree plus global memories;
- pass `org/team` to include `org/team` and descendant scopes plus global;
- pass `all` to explicitly query every scope.

Point tools (`remember`, `read`, `edit`, `move`, and `forget`) address one exact
scope. Omitting their scope targets global.

Scopes organize storage and retrieval. They do not enforce authorization.

## Synchronization

`sync` pulls before pushing by default. It requires a configured remote for
remote work; local-only deployments return without trying to push. Git conflicts
are resolved using the timestamps stored in memory frontmatter, with warnings
recording the resolution.

## Recall feedback

Every recall returns a `recall_id`. Once an agent decides whether a result was
useful, it can report:

- `mark_applied` for one result;
- `batch_mark_applied` for several results in one transaction;
- `recall_stats` to inspect applied, maybe, not-applied, and unknown results by
  distance bucket.

Verdicts are `applied`, `maybe`, or `not_applied`. Confidence is `high`,
`medium`, or `low`. The feedback log is local SQLite telemetry; it is not stored
in the Markdown memory repository or synced through git.

## Timing metadata

Successful MCP tool results include
`_meta["memory-mcp/serverProcessingDurationMs"]`. It measures work from the
server's tool-handler boundary through result conversion. It does not include
network or client processing time.
