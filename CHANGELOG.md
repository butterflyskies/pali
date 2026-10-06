## [Unreleased]

### Added

- **Tag filters on `list` and `recall`** (#148): optional `tags_all` (every
  tag must be present) and `tags_any` (at least one must be present). Matching
  is exact and case-sensitive; an empty or omitted array applies no filter.
  `list` applies the filter before pagination, so `count`, `has_more`, and
  cursors describe the filtered set, and a cursor is rejected under a
  different tag filter (cursors issued without tag filters remain valid).
  `recall` applies it as a pre-filter on both semantic and BM25 candidates
  before ranking is cut to `limit`.
- **`VectorStore::search_bound`**: the smallest search `limit` that ranks
  every raw entry a scope filter can reach, including entries the store drops
  before returning. Tag-filtered recall widens its window up to this bound.
- **Opt-in `content` field for `list`**: `fields: ["content"]` returns each
  memory's full body. The 24 KiB page cap still applies; oversized pages split
  across `next_cursor` instead of truncating bodies.

### Fixed

- **A commit made outside Pali is no longer silently reverted (#365).** `MemoryRepo`
  holds one `Repository` for the process lifetime and libgit2 caches the index it
  returns; nothing re-read it, so a commit made out-of-band left the cached index
  describing a tree that predated it. The next `save`, `delete`, or `move` wrote that
  stale index back over `.git/index`, built its tree from it, and parented the result
  on current HEAD — producing a commit that reads as a wholesale revert of the external
  writer's work with one file added on top. Observed in the wild as a `save memory`
  commit that did `25 files changed, 217 insertions(+), 679 deletions(-)`. The working
  tree stayed correct throughout, so only the repository history was wrong, which is why
  it can go unnoticed for weeks. Write paths now base their index on the current HEAD
  tree, so a Pali commit is always "HEAD, plus exactly what Pali staged." Changes an
  outside writer has staged but not committed are therefore unstaged by the next Pali
  write; the files stay on disk, and the old code unstaged them too. Not
  fail-closed on divergence: a memory store must not answer confusion by declining to
  remember. Merge handling is unchanged — a merge index is supposed to differ from HEAD.

  **Upgrading stops new damage; it does not repair a store that was already hit.** In
  such a store HEAD still lacks the external writer's changes: files the bad commit
  dropped are untracked on disk, its reverted edits show as modified, and files it
  resurrected show as deleted. Pali does not detect or re-add any of this. Because the
  working tree stayed correct, `git status` in the memory checkout lists exactly that
  drift; once you have checked that nothing else is in flight there, stop Pali, run
  `git add -A && git commit`, and HEAD matches the working tree again.

## [0.19.0] - 2026-08-27

### Behavior changes — read before upgrading

- **Runtime environment variables now use the `PALI_*` prefix.** This is a
  clean cutover: every former `MEMORY_MCP_*` runtime/configuration variable is
  renamed to the corresponding `PALI_*` variable, and the old names are no
  longer recognized. Update deployment manifests, container environment,
  secret injection, test harnesses, and other callers at the same time as the
  `0.19.0` binary or image. A known former variable makes startup fail with an
  error naming its replacement rather than silently falling back to a default;
  unknown legacy-prefixed variables are reported only by count. Rollback must
  restore both the pre-0.19 artifact and its former environment names.
  Command-line flags and on-disk paths are unchanged.

## [0.18.0] - 2026-08-22

### Behavior changes — read before upgrading

- **Release assets are renamed from `memory-mcp` to `pali`.** Published release archives change from `memory-mcp-<version>-<target>.tar.gz` (plus `.sha256`) to `pali-<version>-<target>.tar.gz`, and the packaged executable inside changes from `memory-mcp` to `pali`. **Migration:** this is a clean cutover, effective the first `pali`-named release — no compatibility aliases or duplicate `memory-mcp`-named assets are published alongside the new ones. Scripts, CI jobs, or manifests that pin the old asset or executable names must update to the `pali` equivalents before upgrading past that release.

### Added

- **`note` tool alias for `remember`:** both names use the same input schema and
  storage operation. `note` is the preferred canonical name; `remember`
  remains available for compatibility.
- **Federated straddled reads** (ADR-0044): a named Pali instance can query its
  local store and configured sibling MCP endpoints concurrently with bounded
  timeouts. Every fragment retains `store_id` provenance, sibling failures do
  not discard a successful local result, recursive fan-out is prohibited, and
  the caller's bearer token is propagated rather than replaced by ambient
  service credentials. Without caller identity, Pali serves the local read and
  marks siblings `identity_unavailable` without contacting them. Omitting
  `straddle` preserves the existing read response shape.
- **Chunk-addressable retrieval contract** (#262 slice 1, ADR-0042): typed identity and wire shapes for fact-level retrieval units — deterministic `FactId` (parent id + chunker version + source span + content digest, canonical `fact:v1:...` string form), validated non-empty UTF-8 `SourceSpan`, `ChunkerVersion`, and the crate-internal catalog (`FactRecord`) and recall-provenance (`MatchedChunk`) shapes, which go public when their slices wire them. Contract only — no behavioral wiring; the deterministic chunker, derived catalog, chunk indexes, and response wiring land in later slices. `MemoryRef` gains strict `Serialize`/`Deserialize` support for shapes that embed a parent reference. Existing whole-memory retrieval is unchanged. ADR-0042 carries the #262 invariant ledger (each invariant mapped to enforcement, test, and owning slice) and the index-persistence posture; `proptest` lands as a dev-dependency seeding the repository's property-based-testing layer (serde round-trip totality, canonical-form totality, span validity, and collision-resistance evidence over generated inputs).

### Dependencies

- Upgrade `h2` to 0.4.16 to resolve RUSTSEC-2026-0258 (unbounded processing of
  empty DATA frames), refresh the locked dependency graph, and explicitly
  allow the CDLA-Permissive-2.0 license only for `webpki-root-certs` and the
  existing `webpki-roots` package.

## [0.17.2] - 2026-08-06

### Fixed

- **Embedding timeouts no longer cascade into hours of abandoned retries.** The Candle worker now skips queued requests after their callers time out, distinguishes queue delay from active inference, and startup or incremental reindexing does not split worker-queue, saturation, or lifecycle failures into one retry per memory. Active inference timeouts remain splittable because a smaller batch may succeed. A known vector-mirror gap also keeps `/readyz` red instead of being hidden by a later successful index operation.

## [0.17.1] - 2026-07-19

### Fixed

- **Fresh deployments no longer signal ready with an empty semantic index.** On a fresh repository path with a configured remote and `--require-remote-sync`, startup rebuilt and certified the vector index *before* the initial pull — `/readyz` went green while semantic recall missed every memory stored on the remote, for the entire first lifetime of the process. Startup now completes the initial pull before index freshness is decided, so a fresh boot rebuilds and certifies against post-pull git truth before ready is signaled (#328).
- **Per-scope mapped repositories are pulled at startup too.** With per-scope remote mapping configured, mapped repositories were initialized locally without fetching, so a fresh deployment could report ready while every remotely stored mapped-scope memory was absent until an explicit `sync`. Startup now pulls the default repository plus every scope-mapped route (respecting per-route branch overrides), and aggregate sync health settles once from the complete outcome — a failed mapped-remote pull followed by a clean pull no longer reports healthy (#328).

## [0.17.0] - 2026-07-19

### Behavior changes — read before upgrading

- **Repository paths now fail closed on unresolvable components.** `--repo-path`, `MEMORY_MCP_REPO_PATH`, and per-scope config paths are canonicalized at startup; a path component that *exists but cannot resolve* — a dangling symlink, a regular file where a directory is expected, or an untraversable prefix — now aborts startup with an I/O error instead of being silently reclassified as missing and redirecting the repository to a sibling path (#293). **Migration:** if your server previously started with a malformed repo path, it was almost certainly not using the path you intended; fix the path in your config or environment. Genuinely missing path components are still created as before.
- **Sync failures surface as tool errors.** With per-scope remotes configured, `sync` attempts every repository even when one fails (a network blip on one remote no longer blocks the others), but the tool call itself now fails when any repository's pull or push failed — the error enumerates the failed repositories and the structured payload lists which synced cleanly (#293). Clients that treated a `sync` success response as proof of a clean push should rely on this contract rather than log output.
- **Index certification is stricter.** The vector-index freshness stamp is written only when a startup reindex or incremental mirror completes with *zero* item-level errors; a partially populated index is never certified as intact. After a partial failure the next startup runs a repairing full reindex instead of skipping it (#293). Expect an extra reindex pass — that is the repair working as intended, not a regression.
- **Recall results gain a `match_type` field** (`semantic`, `lexical`, or `both`), and lexical-only hits carry a `distance` of `-1.0` as a sentinel (#308). `distance` stays numeric on the wire, so strict clients that model it as a required float keep deserializing — but distance-threshold logic must exclude the `-1.0` sentinel.
- **`list` responses are now paginated** (50 summaries by default, 100 maximum) with cursor pagination, exact field projection, deterministic ordering, and a 24 KiB page ceiling (#302). Existing callers must follow `next_cursor` when `has_more` is true; omitting `fields` retains the prior six-field summary shape. `list.count` now means the total matching memories, while `list.returned` is the number in the current page. *(These entries previously appeared under 0.16.0 in this file, but the feature merged after the v0.16.0 tag — it ships here.)*

### Added

- **Per-scope remote mapping** (#293, ADR-0041): route different scope subtrees to different git repositories, each with its own remote, via a TOML config file (`--config` / `MEMORY_MCP_CONFIG` with `[[remotes]]` entries). Proprietary memories can sync to a private repo while shared memories stay in the common one. Each repo syncs independently; reads aggregate across repos with strict scope-ownership filtering; cross-repo moves preserve memory identity (`id`, `created_at`). Repo-path collisions (including symlink aliases) are rejected at startup, mapped remote URLs are credential-redacted in logs, and mapped branch names are validated. **With no config file, behavior is identical to the previous single-repo mode.**
- **Hybrid recall** (#308, ADR-0038): recall now fuses BM25 lexical search (in-RAM Tantivy index over memory names and content, rebuilt from the repo at startup — no persistence, no migrations) with the existing semantic embedding search via reciprocal rank fusion. Exact-phrase matches rank strictly above term-only matches, and the top lexical hit deterministically outranks every semantic-only candidate — literal phrases buried in long multi-topic memories now surface (#55).
- **Lexical index failure/repair contract** (#314, ADR-0039): git remains authoritative and the lexical index is derived state with no silent divergence — any mirror failure flags the index degraded (recall serves semantic-only, search errors instead of serving stale results) and a background single-flight rebuild repairs it deterministically. Every repository write plus its index mirror runs as a cancellation-shielded unit, so a dropped request can never strand a committed git write unmirrored.
- **Aggregate health reporting** (#293): multi-repo sync and git health settle once from the complete aggregate outcome instead of last-operation-wins, so a failed repository followed by a clean one no longer reports healthy readiness.
- Documentation suite for external users: getting started, configuration, client setup, tool reference, architecture, security, and deployment guides under `docs/`, plus `CONTRIBUTING.md` and a reframed README (#315, #316).

### Changed

- Container image now compiles with the `k8s,otlp` feature set (previously `k8s` only). OTLP export stays passive unless activated with `--otlp-required` / `--otlp-optional`.
- Retain the original public Rust `ListArgs { scope }` DTO for semver compatibility; paginated MCP-only request fields use an internal wire type (#302).

### Fixed

- `--otlp-required` now probes collector reachability at startup (TCP connect, 5s timeout) and fails fast with a non-zero exit before serving when the collector is unreachable; previously the lazy tonic exporter let the server start and errors only surfaced on the first export attempt. "Required" means reachable at startup — later outages are handled by the batch exporter per normal OTLP semantics. `--otlp-optional` remains fully lazy.

### Dependencies

- Bump the rust-dependencies group with 10 updates — tokio 1.53, usearch 2.26, serde 1.0.229, uuid 1.24, and others (#320)
- Bump the GitHub Actions group with 8 updates (#317)

## [0.16.0] - 2026-07-12

### Added

- Return server-side tool processing duration in MCP result metadata and default-verbosity completion logs (#298)
- Report edit-stage totals and separate embedding queue-wait and inference timings (#298)

### Dependencies

- Upgrade `anyhow` to 1.0.103 and `crossbeam-epoch` to 0.9.20 to resolve newly published security advisories

## [0.15.0] - 2026-06-26

### Added

- `move` MCP tool — relocate memories between scopes, preserving content and metadata (#268)
- `batch_mark_applied` MCP tool — submit multiple recall verdicts in a single call for multi-verdict recall feedback (#279)

### Changed

- Consolidate agent instructions into AGENTS.md (#269)
- Remove private repo references from ROADMAP.md (#263)

### Dependencies

- Bump the rust-dependencies group with 2 updates (#265)
- Bump the actions group with 7 updates (#264)
- Batch dependabot dependency updates (#280)

## [0.14.0] - 2026-05-25

### Breaking

- `Scope::Global` renamed to `Scope::Root`; `Scope::Project(String)` replaced with `Scope::Path(ScopePath)` — update all match arms and constructors
- `ScopeFilter::GlobalOnly` renamed to `RootOnly`; `ProjectAndGlobal` replaced with `Subtree(ScopePath)` — update all match arms
- The `"project:{name}"` scope format is no longer accepted in tool input — use bare namespace paths (e.g. `"my-project"` instead of `"project:my-project"`). Existing YAML frontmatter with `type: Project` is still deserialized for migration purposes; a future startup migration (#256) will rewrite these on disk.

### Added

- `ScopePath` validated newtype wrapping a scope path string — construction via `ScopePath::new()` is the sole validation path
- `ValidatedString` trait unifying `ScopePath` and `MemoryName` — shared construction, validation, and deserialization pattern
- Hierarchical path-based namespaces: scope paths may contain `/` for org/team/project nesting (e.g. `"org/team/project"`)
- `ScopeFilter::matches()` method — predicate to test whether a `Scope` passes a filter, usable outside the index layer
- `ScopePath::from_dir()` — derive a validated scope path from a filesystem directory relative to the namespace root
- `Memory::mem_ref()` convenience method — returns a `MemoryRef` without manual field extraction

### Changed

- Canonical index key format changed to `v1:scope=...;name=...` — old `scope=...;name=...` form is still parsed for backward compatibility
- Serde: new variant names (`Root`, `Path`) are serialized; legacy names (`Global`, `Project`) are accepted on deserialisation
- `Scope::dir_prefix()` now returns `Cow<'static, str>` — `Root` avoids allocation, `Path` variants produce an owned string
- `ScopeRegistry` type extracted in index layer — replaces raw `HashMap<Scope, VectorIndex>` with named type and methods
- Lock macros replace scattered `.expect("lock poisoned")` calls in usearch index
- Code and comments use "namespace" terminology; "project" retained only for on-disk backward compat

### Migration guide

Agents using `scope: "project:my-api"` in tool calls must switch to `scope: "my-api"`. The server instructions document the new format; agents that read them will adapt automatically. For agents with hardcoded scope strings in CLAUDE.md or similar config, update the strings manually.

## [0.13.3] - 2026-05-25

### Changed
- Server instructions now guide agents to call `mark_applied` after acting on recalled memories, closing the feedback loop for threshold calibration

## [0.13.2] - 2026-05-25

### Fixed
- git2 0.21 dropped `https` from default features, breaking HTTPS git operations in container deployments (`no TLS stream available`) (#244)
- RecallLog `open()` now creates parent directories before opening the SQLite file

### Dependencies
- git2: enabled `https` + `vendored-openssl` features (required since git2 0.21)

## [0.13.1] - 2026-05-25

### Added
- `mark_applied` MCP tool — agents report whether recalled memories were useful with a verdict tristate (`applied`, `maybe`, `not_applied`) (#213)
- `recall_stats` MCP tool — returns precision statistics bucketed by distance range, accessible over the wire for agent-driven threshold calibration
- `recall-stats` CLI subcommand for local inspection of recall precision data
- Read-recall correlation: `read` handler auto-marks `was_read=1` on recall events for the same session
- `Verdict` enum (`Applied`, `Maybe`, `NotApplied`) replacing boolean `applied` field
- `--recall-log-busy-timeout` CLI flag / `MEMORY_MCP_RECALL_LOG_BUSY_TIMEOUT` env var (default 5s)

### Changed
- `RecallLog` uses connection-per-call pattern — no in-process Mutex, SQLite WAL handles concurrency
- All SQLite calls wrapped in `traced_spawn_blocking` to avoid blocking the async runtime
- `mark_applied` scoped by `session_id` (prevents cross-session tampering)
- `mark_applied` enforces first-call-wins via `WHERE was_applied IS NULL`
- `recall_stats` SQL uses `GROUP BY` aggregation instead of full table scan
- `RecallResult.distance` widened from `f32` to `f64` to prevent bucket boundary imprecision
- `AppState.recall_log` changed from `Option<RecallLog>` to `Option<Arc<RecallLog>>`

### Fixed
- SQL bucketing precision: `CAST(distance * 20 AS INTEGER)` instead of `ROUND(distance / 0.05)` which misplaced f32 boundary values

## [0.13.0] - 2026-05-25

### Added
- `MemoryName` newtype: validated memory name with `FromStr`, `Deserialize`, `JsonSchema` — construction is the sole validation path (#224)
- `MemoryRef` newtype pairing `Scope` + `MemoryName` with `qualified_path()` method (#228)
- `RecallLog`: SQLite-backed append-only event log for recall threshold calibration (#213)
- `recall_id` field in recall responses for correlating results with telemetry
- SIGTERM handler for graceful shutdown alongside existing SIGINT (Ctrl+C) (#195)

### Changed
- `Memory.name` changed from `String` to `MemoryName` (**breaking**) (#224)
- `Memory::new()` now takes `impl Into<String>` for name and content, validates internally, returns `Result<Self, MemoryError>` (#230)
- `Memory::from_validated()` added as `pub(crate)` constructor for pre-validated names
- `AppState::new()` gains `recall_log: Option<RecallLog>` parameter (**breaking**)
- `parse_qualified_name()` returns `MemoryRef` instead of `(Scope, MemoryName)` tuple

### Fixed
- Auth tests isolated from system keyring backends (`DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`) (#225)
- Flaky subprocess integration tests converted to in-process tower oneshot tests (#227)

### Dependencies
- Added `rusqlite` 0.34 (bundled) for recall event logging

## [0.12.1] - 2026-05-18

### Fixed
- `edit` tool now rejects calls that provide neither `content` nor `tags`, returning an `InvalidInput` error instead of silently succeeding with a timestamp-only commit (#219)

## [0.12.0] - 2026-05-14

### Added
- `--idle-timeout-secs` CLI flag / `MEMORY_MCP_IDLE_TIMEOUT_SECS` env var for configurable session idle timeout (default 14400s / 4 hours) (#114)
- `--max-session-lifetime-secs` CLI flag / `MEMORY_MCP_MAX_SESSION_LIFETIME_SECS` env var for absolute session lifetime cap (default disabled) (#114)
- Runtime warning when both idle timeout and max session lifetime are disabled
- Design artifacts in `docs/design/session-lifecycle/`

### Changed
- Session manager construction uses `BoundedSessionManagerBuilder` from mcp-session 0.2
- Structured tracing events emitted on session create/close with `session_id`, `duration_secs`, and `CloseReason`

### Dependencies
- Upgraded `mcp-session` from 0.1 to 0.2 (builder API, max lifetime, lifecycle tracing)
- Upgraded `rmcp` from 1.6 to 1.7

## [0.11.1] - 2026-05-13

### Added
- `/version` HTTP endpoint returning `{"version": "..."}` for runtime version introspection (#204)

## [0.11.0] - 2026-05-10

### Added
- `/readyz` readiness probe endpoint with per-subsystem health reporting (git repo, embedding, vector index, sync)
- `/healthz` liveness probe endpoint (always 200)
- Passive health architecture: subsystems report their own operational state via `SubsystemReporter` backed by `arc-swap` — zero-contention, wait-free reads
- `--require-remote-sync` flag: performs initial pull at startup, includes sync health in readiness checks
- `--health-stale-secs` flag: opt-in staleness detection (disabled by default)
- Transition-based logging: warn on degradation, info on recovery, debug on steady-state polling
- ADR-0027: passive health reporting architecture

### Changed
- `CandleEmbeddingEngine::new()` now accepts a `SubsystemReporter` parameter
- `UsearchStore` gains `new_with_reporter()` and `load_with_reporter()` constructors
- `MemoryRepo` gains `init_or_open_with_reporter()` constructor with git + sync reporters
- `AppState::new()` now accepts a `HealthRegistry` parameter
- `read_memory` no longer marks git subsystem degraded on `NotFound` / `InvalidInput` errors
- Startup health reporting is conditional on reindex success
- Startup validation: `--require-remote-sync` without `--remote-url` is rejected immediately

### Dependencies
- Added `arc-swap` 1.x (wait-free atomic pointer swap for health state)

## [0.10.1] - 2026-05-10

### Added
- `--allowed-host` CLI arg / `MEMORY_MCP_ALLOWED_HOST` env var for DNS rebinding protection bypass behind reverse proxies
- `--version` flag via clap
- Integration tests for rmcp host header validation (accepted, rejected, allowed)

### Fixed
- Server rejected requests via reverse proxy due to rmcp DNS rebinding protection blocking unrecognized `Host` headers

## [0.10.0] - 2026-05-10

### Added
- Startup reindex: compare persisted index SHA against repo HEAD, rebuild from scratch on mismatch (#193)
- `full_reindex` public function for crash recovery and startup freshness checks
- `MemoryRepo::head_sha()` async method for reading HEAD commit SHA
- `--embed-timeout-secs` CLI arg (default 30, env `MEMORY_MCP_EMBED_TIMEOUT_SECS`)
- `--embed-queue-size` CLI arg (default 64, env `MEMORY_MCP_EMBED_QUEUE_SIZE`)
- `parse_nonzero_u64` validator with tests
- Worker thread `Drop` impl that closes channel and joins thread

### Changed
- **Breaking:** `CandleEmbeddingEngine::new()` now takes `(Duration, usize)` parameters
- **Breaking:** `CandleEmbeddingEngine` no longer implements `UnwindSafe`/`RefUnwindSafe`
- Replace mutex+spawn_blocking embedding design with dedicated worker thread + bounded channel (#192)
- Embed calls self-heal after timeouts — worker continues processing next request
- `catch_unwind` in worker loop survives panics with tokenizer state recovery
- `head_sha()` uses `spawn_blocking` consistent with other `MemoryRepo` methods
- Startup reindex discards loaded index to prevent ghost entries from deleted memories
- SHA stamped on shutdown save and after sync pulls to prevent spurious reindexes
- Upgrade OpenTelemetry stack to 0.31 and tokenizers to 0.23
- Bump the rust-dependencies group with 2 updates

### Fixed
- Embedding timeout: hung candle inference no longer blocks all future embed calls (#192)
- Crash recovery: index rebuilt automatically after kill -9, no more empty recall results (#193)
- IPv6 localhost exception and review findings from #178
- Record count on local span to prevent parent span leak
- Pre-existing clippy lints from Rust 1.95 (`manual_contains`, `single_match`)

## [0.7.1] - 2026-04-21

### Changed
- Bump hf-hub from 0.4.3 to 0.5.0

## [0.7.0]

### Added
- Add `native-certs` feature for OS certificate store support by @spolom in [#143](https://github.com/butterflyskies/memory-mcp/pull/143)

### New Contributors
* @spolom made their first contribution in [#143](https://github.com/butterflyskies/memory-mcp/pull/143)

## [0.6.1]

### Fixed
- Atomic file writes with RAII cleanup and symlink defense
- Drop `--locked` from `cargo publish`

### Changed
- Refresh `Cargo.lock` to unblock v0.6.0 release

## [0.6.0]

### Added
- Add `PushRejected` error variant and integration test

### Changed
- Mark `MemoryError` as `#[non_exhaustive]` (breaking: downstream `match` must add `_ =>`)

### Fixed
- Surface server-side push rejections instead of silently succeeding
- rmcp 1.4 compat and dependency refresh

## [0.5.1]

### Added
- Add secret-avoidance guidance to MCP tool instructions
- Add docker run quick-start section to README
- Add development roadmap

## [0.5.0]

### Changed
- Supply chain audit — reduce deps, vendor native libs, unify TLS

## [0.4.0]

### Added
- Address final review P3 findings
- Address review findings for partitioned indexes
- Add unit tests for ScopeFilter::matches()
- Address code review findings
- Add workflow_dispatch to release workflow by @butterflysky-ai in [#89](https://github.com/butterflyskies/memory-mcp/pull/89)
- Add trusted publishing for crates.io releases by @butterflysky-ai in [#77](https://github.com/butterflyskies/memory-mcp/pull/77)
- Add deployment workstream plan and update project memories by @butterflysky-ai in [#76](https://github.com/butterflyskies/memory-mcp/pull/76)
- Add MCP client config examples for all major editors by @butterflysky-ai in [#75](https://github.com/butterflyskies/memory-mcp/pull/75)
- Add cargo-semver-checks as required CI job by @butterflysky-ai in [#73](https://github.com/butterflyskies/memory-mcp/pull/73)

### Changed
- Bump version to 0.4.0, revert workflow changes
- Scope-partitioned vector indexes
- Scope affinity for recall and list
- Skip redundant verification build in publish-crate by @butterflysky-ai in [#91](https://github.com/butterflyskies/memory-mcp/pull/91)
- Release 0.3.1 by @butterflyskies-release-manager-bot[bot] in [#90](https://github.com/butterflyskies/memory-mcp/pull/90)
- Bounded session management via mcp-session by @butterflysky-ai in [#82](https://github.com/butterflyskies/memory-mcp/pull/82)

### Fixed
- Serialise ScopedIndex add/remove with write lock
- Harden index persistence and remove dead code
- Fix README accuracy and update TODO to reflect current state by @butterflysky-ai in [#74](https://github.com/butterflyskies/memory-mcp/pull/74)

### Removed
- Remove dead ensure_scope, document lock ordering
- Remove release-please workflow and configuration by @butterflysky-ai in [#95](https://github.com/butterflyskies/memory-mcp/pull/95)

## [0.3.0] - 2026-03-21

### Changed
- Release 0.3.0 by @butterflyskies-release-manager-bot[bot] in [#58](https://github.com/butterflyskies/memory-mcp/pull/58)
- Fat lib / thin binary — expose domain modules from lib.rs by @butterflysky-ai in [#64](https://github.com/butterflyskies/memory-mcp/pull/64)
- Update project overview with trust signals and release infrastructure by @butterflysky-ai in [#63](https://github.com/butterflyskies/memory-mcp/pull/63)
- Trust signals phase 1 — metadata, cargo-deny, rustdoc by @butterflysky-ai in [#59](https://github.com/butterflyskies/memory-mcp/pull/59)
- Update project overview and add operational concerns memory by @butterflysky-ai in [#54](https://github.com/butterflyskies/memory-mcp/pull/54)

### Fixed
- Use GitHub App token for release-please to trigger CI by @butterflysky-ai in [#57](https://github.com/butterflyskies/memory-mcp/pull/57)

### New Contributors
* @butterflyskies-release-manager-bot[bot] made their first contribution in [#58](https://github.com/butterflyskies/memory-mcp/pull/58)

## [0.2.0] - 2026-03-20

### Changed
- Release 0.2.0 by @github-actions[bot] in [#53](https://github.com/butterflyskies/memory-mcp/pull/53)
- Replace fastembed with candle direct for pure-Rust embeddings by @butterflysky-ai in [#51](https://github.com/butterflyskies/memory-mcp/pull/51)
- Update project overview memory with cross-platform CI details by @butterflysky-ai in [#47](https://github.com/butterflyskies/memory-mcp/pull/47)

## [0.1.5] - 2026-03-19

### Changed
- Release 0.1.5 by @github-actions[bot] in [#46](https://github.com/butterflyskies/memory-mcp/pull/46)
- Vendor OpenSSL and add cross-platform compilation checks by @butterflysky-ai in [#43](https://github.com/butterflyskies/memory-mcp/pull/43)

### Fixed
- Vendor OpenSSL only on non-Linux platforms by @butterflysky-ai in [#45](https://github.com/butterflyskies/memory-mcp/pull/45)

## [0.1.4] - 2026-03-19

### Changed
- Release 0.1.4 by @github-actions[bot] in [#39](https://github.com/butterflyskies/memory-mcp/pull/39)
- Release 0.1.5 by @github-actions[bot] in [#36](https://github.com/butterflyskies/memory-mcp/pull/36)
- Release 0.1.4 by @github-actions[bot] in [#35](https://github.com/butterflyskies/memory-mcp/pull/35)
- Release 0.1.4 by @github-actions[bot] in [#32](https://github.com/butterflyskies/memory-mcp/pull/32)
- Release 0.1.5 by @github-actions[bot] in [#29](https://github.com/butterflyskies/memory-mcp/pull/29)
- Release 0.1.4 by @github-actions[bot] in [#27](https://github.com/butterflyskies/memory-mcp/pull/27)

### Fixed
- Upgrade release-please-action for force-tag-creation support by @butterflysky-ai in [#38](https://github.com/butterflyskies/memory-mcp/pull/38)
- Restore release-please labels and reset version to v0.1.3 by @butterflysky-ai in [#34](https://github.com/butterflyskies/memory-mcp/pull/34)
- Clean up orphaned release state and skip labeling by @butterflysky-ai in [#31](https://github.com/butterflyskies/memory-mcp/pull/31)
- Move doc comment above #[cfg] so clap shows help for k8s-secret store by @butterflysky-ai in [#28](https://github.com/butterflyskies/memory-mcp/pull/28)
- Use draft releases so binary assets can be uploaded before publish by @butterflysky-ai in [#26](https://github.com/butterflyskies/memory-mcp/pull/26)

## [0.1.3] - 2026-03-18

### Added
- Add release binary assets with SHA256 checksums by @butterflysky-ai in [#22](https://github.com/butterflyskies/memory-mcp/pull/22)

### Changed
- Release 0.1.3 by @github-actions[bot] in [#25](https://github.com/butterflyskies/memory-mcp/pull/25)

### Fixed
- Move doc comments above #[cfg] so clap shows help text for k8s flags by @butterflysky-ai in [#24](https://github.com/butterflyskies/memory-mcp/pull/24)
- Fix cargo-binstall version pin and quiet gh run watch by @butterflysky-ai in [#21](https://github.com/butterflyskies/memory-mcp/pull/21)

## [0.1.2] - 2026-03-18

### Changed
- Release 0.1.2 by @github-actions[bot] in [#20](https://github.com/butterflyskies/memory-mcp/pull/20)

### Fixed
- Fix release image tags and gate publish on CI by @butterflysky-ai in [#19](https://github.com/butterflyskies/memory-mcp/pull/19)

## [memory-mcp-v0.1.1] - 2026-03-18

### Added
- Add comprehensive README by @butterflysky-ai in [#15](https://github.com/butterflyskies/memory-mcp/pull/15)
- Add Kubernetes deployment (Round 1) by @butterflysky-ai in [#13](https://github.com/butterflyskies/memory-mcp/pull/13)
- Add release-please and PR title linting by @butterflysky-ai in [#14](https://github.com/butterflyskies/memory-mcp/pull/14)
- Add --store k8s-secret backend for auth login (#k8s feature) by @butterflysky-ai in [#12](https://github.com/butterflyskies/memory-mcp/pull/12)
- Add keyring-based token storage as auth fallback by @butterflysky-ai in [#9](https://github.com/butterflyskies/memory-mcp/pull/9)
- Add ADR-0010: keyring-based token storage by @butterflysky-ai in [#5](https://github.com/butterflyskies/memory-mcp/pull/5)

### Changed
- Release memory-mcp 0.1.1 by @github-actions[bot] in [#18](https://github.com/butterflyskies/memory-mcp/pull/18)
- Migrate to googleapis/release-please-action and bump action versions by @butterflysky-ai in [#17](https://github.com/butterflyskies/memory-mcp/pull/17)
- Update project overview memory by @butterflysky-ai in [#16](https://github.com/butterflyskies/memory-mcp/pull/16)
- Update project overview and session handoff memories by @butterflysky-ai in [#11](https://github.com/butterflyskies/memory-mcp/pull/11)
- Implement auth subcommand with OAuth device flow by @butterflysky-ai in [#10](https://github.com/butterflyskies/memory-mcp/pull/10)
- Incremental index rebuild on pull by @butterflysky-ai in [#7](https://github.com/butterflyskies/memory-mcp/pull/7)
- Modify funding sources in FUNDING.yml by @butterflysky in [#8](https://github.com/butterflyskies/memory-mcp/pull/8)
- Update TODO.md to reflect Phase 2 progress by @butterflysky-ai in [#6](https://github.com/butterflyskies/memory-mcp/pull/6)
- Implement git push/pull with auth and conflict resolution by @butterflysky-ai in [#4](https://github.com/butterflyskies/memory-mcp/pull/4)
- Update TODO.md to reflect current project status by @butterflysky-ai in [#3](https://github.com/butterflyskies/memory-mcp/pull/3)
- Implement all 7 MCP tool handlers with full observability by @butterflysky-ai in [#2](https://github.com/butterflyskies/memory-mcp/pull/2)
- Scaffold Rust MCP server with streamable HTTP transport by @butterflysky-ai in [#1](https://github.com/butterflyskies/memory-mcp/pull/1)
- Seed project: git-backed semantic memory MCP server by @butterflysky

### New Contributors
* @github-actions[bot] made their first contribution in [#18](https://github.com/butterflyskies/memory-mcp/pull/18)
* @butterflysky-ai made their first contribution in [#17](https://github.com/butterflyskies/memory-mcp/pull/17)
* @butterflysky made their first contribution in [#8](https://github.com/butterflyskies/memory-mcp/pull/8)
