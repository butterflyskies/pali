# Pali Roadmap

> Canonical source of truth for the Pali development plan.
> Updated with each epic completion. See the [pinned roadmap issue](https://github.com/butterflyskies/pali/issues/132) for discussion.

## What Pali is

A hybrid memory system for AI coding agents. Memories are stored as markdown files in a git repository, indexed locally for semantic and BM25 lexical retrieval, and synced to a remote. It ships as a single Rust binary speaking MCP over Streamable HTTP.

## Where it's heading

The project has moved from phase-based planning to **value-based epics**. The old phases (1-7) mapped roughly to build order; epics map to *why the work matters*. Work within an epic can proceed in any order as dependencies allow.

**Current priority:** Retrieval quality, starting with the HyperMem bridge (#262).

---

## Epics

### Retrieval Quality
Make recall find the right thing.

The highest-priority epic. Pali#262 (HyperMem bridge — graph-aware chunk index) is the current focus, providing a path to structured chunking and hybrid retrieval without rewriting the existing index.

| Issue | Title | Status |
|-------|-------|--------|
| #262 | Graph-aware chunk index for retrieval (HyperMem bridge) | **Active** |
| #141 | Upgrade to ModernBERT Embed (8192 token context) | Open |
| #140 | Chunk long memories for embedding | Open |
| #55 | BM25 keyword search via Tantivy | Completed in #308 |
| #148 | Tag-based filtering in recall (and list) | Completed in #370 |
| #197 | Threshold-based recall: auto-expand until relevance drops | Open |
| #129 | Memory consolidation, write discipline, quality metrics | Open |
| #147 | Deduplication / update detection on remember | Open |
| #107 | Memory expiry: TTL + completion-triggered deletion | Open |

### Recall Telemetry
Measure whether retrieval works.

The `mark_applied` feedback loop is live. This epic builds on it with structured telemetry, analytics by memory name, and schema normalization to make the data actionable.

| Issue | Title | Status |
|-------|-------|--------|
| #213 | Recall evaluation log (applied-memory telemetry) | Open |
| #241 | Recall telemetry v2 — retrieval funnel, shadow policies, filter regret | Open |
| #247 | Memory health analytics: index recall verdicts by memory name | Open |
| #251 | Normalize recall telemetry schema (recall_batches + recall_results) | Open |
| #253 | Review finding telemetry — pattern fire rates, FP rates | Open |
| #260 | Clean up orphaned recall log rows on memory delete/edit | Open |

### Multi-Agent Access Control
Support multiple identities with scope isolation.

Enables different agents to share a Pali instance without leaking across scope boundaries.

| Issue | Title | Status |
|-------|-------|--------|
| #261 | Scope access control — public, shared, private scopes per identity | Open |
| #259 | Scope access control and authorization enforcement | Open |
| #116 | Memory scope isolation for multi-user deployments | Open |
| #115 | Authentication and authorization framework | Open |
| #113 | Log access denied events | Open |

### Metadata Framework
Structured data on memories.

Adds classification, enrichment fields (last-accessed, confidence), and stable UUID-based access.

| Issue | Title | Status |
|-------|-------|--------|
| #258 | Metadata framework implementation tracker | Open |
| #248 | Information classification metadata | Open |
| #149 | Memory metadata enrichment (last-accessed, access count, confidence) | Open |
| #257 | read_by_id MCP tool — stable memory access by UUID | Open |

### Observability
Understand what the system is doing.

Builds on the tracing scaffold shipped in v0.8.0 (Phase 2). Completes span coverage, adds audit channels, metrics export, and distributed tracing.

| Issue | Title | Status |
|-------|-------|--------|
| #172 | Complete operational span coverage from #52 | Open |
| #173 | Tiered audit channel infrastructure | Open |
| #162 | W3C Trace Context propagation | Open |
| #165 | Prometheus metrics endpoint (/metrics) | Open |
| #205 | /stats endpoint for operational introspection | Open |
| #110 | Authenticated user identity in log events | Open |
| #111 | Log returned memory names on recall | Open |
| #112 | Log auth events | Open |

### Infrastructure & CI
Build, ship, secure.

CI pipeline, cross-platform support, dependency maintenance, supply chain security. Windows support (#42, #56) remains deferred until demand materializes.

| Issue | Title | Status |
|-------|-------|--------|
| #243 | Cross-compile artifact in release docker image build | Open |
| #78 | validate subcommand + Docker build speedup | Open |
| #98 | Automate CHANGELOG without bypassing branch protection | Open |
| #153 | CVE scanning gate in CI (Grype/Trivy) | Open |
| #152 | Container signing with cosign | Open |
| #79 | GitHub App installation token auth | Open |
| #42 | Windows: usearch MAP_FAILED | Open |
| #56 | Cross-platform vector index (brute-force fallback) | Open |
| #144 | Migrate ureq v2 to v3 | Open |
| #161 | Revisit candle fork patch | Open |
| #160 | docs.rs metadata | Open |
| #214 | hf-hub native-certs gap | Open |
| #194 | candle ARM64 macOS infinite loop | Open |
| #183 | Keep-alive timeout after laptop sleep | Open |
| #166 | Flaky test: large_batch_is_chunked | Open |

### Transport & Deployment
How it runs.

Stdio transport for local single-user deployments, non-GitHub remote support, and deployment documentation.

| Issue | Title | Status |
|-------|-------|--------|
| #104 | Stdio transport | Open |
| #103 | Non-GitHub git remotes | Open |
| #118 | Deployment checklist | Open |
| #150 | Periodic background sync | Open |

### Agent Workflow
Tooling for how agents use Pali.

Compaction hooks, migration paths, CLI access, and internal code quality.

| Issue | Title | Status |
|-------|-------|--------|
| #130 | Post-compaction context re-priming hook | Open |
| #254 | Dedicated test quality sub-agent in /code-review | Open |
| #151 | Migration tools for Serena/auto-memories | Open |
| #154 | CLI for manual memory management | Open |
| #256 | Lazy directory migration from projects/ to namespaces/ | Open |
| #255 | Idiomacy cleanup on in_memory.rs | Open |

---

## Completed Work

### Phase 1: Stabilize & Quick Wins (v0.6.0-v0.6.1)

All 6 issues closed. #88 (flaky test), #81 (git push reject), #69 (atomic writes), #106 (recall truncation), #108 (secret-avoidance), #105 (docker docs).

### Phase 2: Tracing Scaffold + Core Quality (partial)

Tracing scaffold (#52), vector index trait (#94), DeviceFlowProvider (#145), embed timeout (#192), startup reindex (#193), /readyz (#164) all shipped across v0.8.0-v0.11.0.

### Pre-Roadmap (v0.1.0-v0.5.0)

MCP server, streamable HTTP, memory file format, all core tools, local embedding (candle/BGE-small), HNSW index (usearch), git sync, keyring auth, OAuth device flow, K8s manifests, container image, crates.io publishing.

---

## Historical Phase Mapping

For reference, the original phase structure mapped to epics as follows:

| Old Phase | Epic(s) |
|-----------|---------|
| Phase 1 (Stabilize) | Completed |
| Phase 2 (Tracing + Quality) | Observability, Infrastructure & CI |
| Phase 3 (Transport/Stdio) | Transport & Deployment |
| Phase 4 (Search/Lifecycle) | Retrieval Quality, Metadata Framework |
| Phase 5 (Audit Logging) | Observability |
| Phase 6 (Enterprise) | Multi-Agent Access Control |
| Phase 7 (Windows/polish) | Infrastructure & CI |

Issues not in the original phases (filed after the roadmap was written) have been assigned to epics based on their value area.
