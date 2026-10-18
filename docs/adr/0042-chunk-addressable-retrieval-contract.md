# ADR-0042: Chunk-addressable retrieval contract

## Status
Accepted

Design provenance: `chunk-retrieval-262-default-forward-design`
(collective-conscious, scope `infra/memory-mcp`, Syne, 2026-07-18) and
issue #262. This ADR records slice 1 of that design: identity,
addressing, versioning, and wire-compatibility decisions. Later slices
implement behavior against this contract.

## Context
Retrieval currently indexes one vector and one lexical document per
memory. Two measured failure modes motivate finer-grained units:

- **Embedding truncation.** The BGE-small tokenizer truncates at 512
  tokens (`src/embedding/candle.rs`); everything past that contributes
  nothing to the document vector. Measured 2026-05-25: 18 of 191
  memories exceeded the limit and were silently clipped.
- **Fact dilution.** A one-line fact inside a long multi-topic memory
  contributes almost nothing to the whole-document vector. ADR-0038's
  hybrid retrieval fixed the literal-phrase case (#55); topical recall
  of buried facts still fails.

Issue #262 scopes a chunk-addressable retrieval substrate. The design
memory bounds it explicitly: memory-mcp builds a **policy-neutral**
substrate; dione #138 separately consumes authorized retrieval and
injects advisory "this rings a bell" metadata. Neither waits for the
other.

Alternatives considered:
- **Bigger embedding budget** (longer-context model): moves the cliff,
  keeps whole-memory dilution, forces a model migration to fix a data
  modelling problem.
- **Overlapping sliding windows**: standard RAG practice, but
  non-deterministic unit boundaries under edits, duplicated text in the
  index, and no stable identity for telemetry or feedback.
- **Structural chunking with deterministic identity** (chosen): units
  follow markdown structure, identity is derived, the catalog is
  rebuildable from git truth.

## Decision

### Chunk model
Retrieval works over **facts**: chunks of a parent memory's markdown
body. Markdown files remain the sole source of truth; every chunk
artifact is derived, rebuildable state (same posture as the vector and
lexical indexes). The catalog of facts is a derived artifact keyed by
`FactId`, carrying parent identity, source span, heading path, body,
tags, and extracted references.

Chunking (slice 2) is deterministic: split on structural boundaries
(headings, paragraphs, lists, code blocks), coalesce adjacent blocks
under the *actual embedding tokenizer's* budget, deterministically
split oversized blocks, no overlap initially, one chunk for short
memories. Dense and lexical indexes consume the same units.

### Identity and addressing
A `FactId` is derived from the parent memory's id, the chunker
version, the source span, and a SHA-256 digest over all of those plus
the chunk body. Canonical form:

```
fact:v1:<parent-id>:<chunker-version>:<start>-<end>:<digest16>
```

- Deterministic: the same parent content under the same chunker
  version always yields the same ids — rebuilds are idempotent and
  index entries stably addressable.
- The parent id is embedded verbatim, so parent-wide operations
  (delete/update every chunk of one memory, slice 4) filter ids
  without a lookup table.
- The span participates in identity: two identical paragraphs at
  different positions are distinct facts.
- Spans are half-open byte ranges into the parent's markdown body
  (not the on-disk file with frontmatter), non-empty, and resolvable
  only on UTF-8 character boundaries (`SourceSpan::slice_in`).
- The `fact:v1:` prefix versions the id format itself, mirroring the
  `v1:scope=...;name=...` convention for qualified memory keys.

### Versioning and rebuild
`ChunkerVersion` names the chunking algorithm revision. Bumping it
invalidates every derived `FactId` and forces a full catalog rebuild —
there are no chunk migrations, only rebuilds from git truth. Catalog
staleness detection (schema/model/commit) is slice 3.

### Ranking and collapse
Semantic and lexical retrieval both rank facts; fusion (RRF, per
ADR-0038) happens at `FactId`. After fusion, results **collapse to the
best fact per parent** before result limits apply, so a long memory
cannot flood the result list with its own chunks. Slice 6 implements
this with bounded overfetch.

Under an evaluation whose pass rule was fixed before scoring, the whole-memory + chunk (W+C) RRF arm does not go into recall (on set C the fused variant's top-5 recall dropped by 0.110, 95% CI [−0.180, −0.050], on the before-cut half and was unchanged, +0.000, after the cut); a separate chunk-backed semantic path comes first, and fusion stays a swappable policy.

### Wire compatibility (additive)
Existing recall behavior and response fields are unchanged. When chunk
retrieval is wired (slice 7), recall hits additively gain matched-chunk
provenance (`MatchedChunk`: fact id, heading path, matched text).
`read` continues to return the whole parent memory. Clients that
ignore unknown fields keep working; nothing existing is renamed,
removed, or re-typed. The whole-memory index keeps working throughout —
chunking is additive, and each slice ships behind the existing
contract.

### Contract boundary with dione #138
The substrate exposes retrieval with honest provenance (ids, ranks,
match types, distances) and nothing else. Authorization, advisory
framing, audience/directedness policy, and injection live entirely on
the consumer side. Scope names are namespaces, not disclosure
authority; nothing in this contract grants one.

## Operational implications: index persistence

Chunking multiplies index entries, so the persistence posture of each
index becomes a capacity question. Current posture, from the code:

- **Git repo** — durable truth. Everything below is a derived cache.
- **Vector index (usearch)** — persisted to disk under
  `<repo>/.memory-mcp-index/` (`index.usearch` + key map JSON), loaded
  at startup and checked for freshness against repo HEAD via a stored
  commit SHA (`src/index/usearch.rs`, `src/main.rs`). Persistence
  exists because embedding is expensive. #293 tightened certification:
  the freshness stamp is written only when a startup reindex or
  incremental mirror completes with zero item-level errors; a partial
  index is never certified, and an interrupted save (`.save-in-progress`
  marker) discards the on-disk state and rebuilds.
- **Lexical index (tantivy/BM25)** — **RAM only, never persisted.**
  `Index::create_in_ram` in `src/search/bm25.rs`; rebuilt on every
  startup from one repository listing (`rebuild_lexical_from_repo`).
  ADR-0038 chose this deliberately: indexing text is cheap (unlike
  embedding it), and never persisting removes the entire
  index-versioning/migration surface. Degradation and repair follow
  ADR-0039.

So: no, the tantivy index does not persist to disk — by decision, not
omission. Is that a problem? At current corpus size, no.

**How chunking changes the math.** Fact-level units multiply entries
roughly an order of magnitude (~191 memories today → low thousands of
chunks). For the vector index nothing changes structurally — it is
already persisted, and the #293 certification and ADR-0039 repair
contracts apply per entry regardless of count. For the lexical index,
startup rebuild cost and RAM footprint both scale with entry count and
total text. Total indexed text is roughly constant (the same bytes,
sliced finer), so the dominant costs — tokenization and posting-list
construction — grow modestly, not tenfold; per-document overhead is
what multiplies.

**Options going forward.**
1. **Keep in-RAM rebuild-on-boot** (status quo): zero persistence
   surface, zero migrations, one repo pass at startup.
2. **Tantivy `MmapDirectory` on the persistent volume**: near-instant
   startup and lower resident RAM, at the price of re-acquiring
   exactly what ADR-0038 eliminated — an on-disk format to version,
   stamp against repo HEAD, certify (the #293 zero-error discipline),
   and repair on divergence (ADR-0039 machinery, now with a disk
   state to distrust).

**Decision: stay with option 1; deferral trigger documented.** A
low-thousands-document corpus is orders of magnitude below tantivy's
problem size; the rebuild is a single commit on the blocking pool and
posting lists over a few MB of text are negligible next to the
embedding model already resident. Revisit when either (a) the startup
lexical rebuild materially delays readiness (order of seconds, not
milliseconds — instrument it in slice 5), or (b) the corpus reaches
roughly tens of thousands of chunks / tens of MB of indexed text.
Crossing the trigger means adopting `MmapDirectory` *with* the same
commit-SHA stamp and zero-error certification discipline the vector
index already carries — not an ad-hoc cache file.

## Invariants and properties

This section is the property ledger for #262: every contract invariant
is enumerated here, mapped to its enforcement mechanism, its
property-or-example test, and the slice that owns it. Tests reference
these ids; shrunk property-test counterexamples graduate to named
example regressions.

### Ledger

| Invariant | Enforcement mechanism | Test | Slice |
|---|---|---|---|
| Markdown/git is canonical; every catalog and index is disposable derived state | Architecture posture: derived artifacts carry no authoritative state and every one has a rebuild-from-repo path (ADR-0038/0039 machinery today; atomic catalog rebuild in slice 3) | P4.2 rebuild idempotence; existing ADR-0039 repair tests | all (built: 3–5) |
| Fact identity is deterministic, versioned, and domain-separated by parent / version / span / content digest | `FactId::derive`: pure function over a length-prefixed canonical encoding; version and span embedded in the id; derive rejects a body whose byte length disagrees with the span | P1.1 (property + example), P1.2, P1.8 | 1 |
| `FactId` canonical form is bijective on the accepted domain: `parse(render(id)) == id` and `parse(s) = Ok(id) ⇒ render(id) == s` | Strict `FromStr` with per-field canonical numeric parsing — noncanonical spellings (leading zeros, signs) are rejected, never normalized | P1.3 (both-direction properties + rejected-spelling examples) | 1 |
| Catalog records and matched-chunk provenance carry true provenance: the id re-derives exactly from the paired body/text, enforced at construction *and* on deserialize (corrupt persisted state fails closed) | `FactRecord::new` / `MatchedChunk::new` validating constructors; `serde(try_from)` raw-shape deserialization re-runs the same validation; `FactId::derive` span/body length gate | P1.8 (constructor rejections, mutation regressions, generated-mutation property) | 1 |
| Spans are valid UTF-8 byte boundaries, ordered, inside the parent, reproducing the represented source | `SourceSpan` construction invariant + `slice_in` validation (slice 1); chunker emission discipline (slice 2) | P1.5 (property + example); P2.2–P2.4 | 1–2 |
| Same source + version ⇒ same facts/ids; changed identity input ⇒ changed id | Deterministic chunker feeding deterministic derivation; collision posture per P1.2 | P1.1, P1.2, P2.1, P2.7 | 1–2 |
| Rebuild from identical truth is idempotent; stale schema/model/commit is detectable | Versioned derived catalog with staleness stamps (schema, model, commit) | P4.2; slice-3 staleness tests | 3–4 |
| Dense and lexical indexes index the same FactIds | Single catalog is the sole feed for both index builders | slice 4–5 integration tests | 4–5 |
| Parent update/delete removes every prior fact (no ghosts) | Parent-wide delete keyed on the parent id embedded in every `FactId` | P4.1 | 4 |
| Fusion collapses to ≤ 1 hit per parent before the public limit applies | Deterministic collapse stage ordered before limit truncation, with bounded overfetch | P6.1, P6.2 | 6 |
| Wire evolution is additive until an explicit breaking release | New response data lands as new optional fields; deserializers ignore unknown fields; CI semver gate | P1.6, P7.1 | 1, 7 |
| No raw memory content (query/name/heading/body) in *default* telemetry; content-bearing diagnostics exist only behind explicit operator opt-in with separate access controls, sampling, time-bounding, short retention, and a visible active-capture indicator | Default telemetry schema admits only ids (UUID/`FactId`), ranks, match type, token/byte counts, latency, truncation flag, error class, and optionally a deployment-salted query fingerprint; the opt-in diagnostic path is gated and indicated (tracing rules R-16–R-18) | P7.2 | 7 |
| Graph traversal does not exist before an authorization boundary; denied is indistinguishable from missing | `refs_out` stores inert strings; the substrate exposes no resolution/traversal API until slice 9 | API-surface review now; slice-9 denied≡missing tests | 8–9 |

### Slice 1 — contract types (tested now, `src/types/chunk_tests.rs`)

- **P1.1 Determinism.** `FactId::derive` is a pure function: identical
  inputs always yield identical ids (and identical rendered strings).
- **P1.2 Domain separation and collision posture.** The digest input
  is a length-prefixed canonical encoding of (parent id, version,
  span, body) — no two distinct component tuples encode to the same
  byte stream. This is *not* an injectivity claim for the id: the
  rendered digest is truncated to 64 bits, and no property test can
  prove hash injectivity. The true invariant is deterministic
  canonical input + domain separation + explicit collision handling:
  within one catalog generation, ids are structurally unique even
  under digest collision (parent, version, and span are embedded
  verbatim, and two chunks of one parent never share a span); a
  digest collision can therefore only equate a fact with a *prior
  generation* of the same span, and parent-wide update/delete (P4.1)
  removes prior generations entirely before new ones land. Property
  tests assert determinism and distinctness over generated inputs as
  collision-resistance *evidence*, not proof.
- **P1.3 Canonical-form bijectivity.** `parse(render(id)) == id` for
  every derivable id, and acceptance implies canonicality in the other
  direction: `parse(s) = Ok(id) ⇒ render(id) == s`, byte for byte. The
  parser rejects every string `render` cannot produce — including
  noncanonical numeric spellings (leading zeros, signs) that Rust's
  integer `FromStr` would silently normalize — no lenient mode
  (ADR-0019).
- **P1.4 Serde round-trip totality.**
  `deserialize(serialize(x)) == x` for `FactId`, `SourceSpan`,
  `ChunkerVersion`, `FactRecord`, `MatchedChunk`, and `MemoryRef`.
- **P1.5 Span validity.** `start < end` always holds — enforced at
  construction and on deserialization; no `SourceSpan` value can
  exist outside the invariant. `slice_in` resolves a span only when
  it is in bounds and on UTF-8 character boundaries; otherwise it
  errors rather than serving corrupt provenance.
- **P1.6 Additive tolerance.** Wire shapes ignore unknown fields, so
  future additive fields cannot break existing consumers.
- **P1.7 Version ordering.** `ChunkerVersion` ordering coincides with
  numeric ordering of the revision — monotonically increasing
  revisions compare monotonically.
- **P1.8 True provenance.** A `FactId` describes exactly the body it
  was derived from: `derive` rejects a body whose byte length disagrees
  with the span, and `FactRecord` / `MatchedChunk` values cannot exist
  — via constructor or deserialization — unless the id re-derives
  exactly from the paired body/text. Persisted catalog corruption or a
  future chunker bug fails closed on load instead of serving false
  provenance. Test generators build body/span first and derive the id;
  they never pair independently generated ids and bodies.

### Slice 2 — deterministic chunker

- **P2.1 Determinism.** Same parent content + same chunker version →
  byte-identical chunk sequence (spans, bodies, ids).
- **P2.2 Span soundness.** Every emitted span is in bounds and
  resolves on UTF-8 character boundaries within the parent body
  (`slice_in` succeeds for every chunk).
- **P2.3 Ordered non-overlap.** Emitted spans are strictly ordered by
  start and pairwise non-overlapping.
- **P2.4 Coverage.** Concatenating the chunk bodies in span order
  reproduces the parent content with no loss and no duplication,
  modulo the chunker's documented boundary-whitespace handling.
- **P2.5 Short-input degeneration.** Content under the tokenizer
  budget yields exactly one chunk spanning the whole body.
- **P2.6 Budget respect.** Every chunk fits the actual embedding
  tokenizer's budget, except indivisible atomic blocks, whose
  fallback behavior must be documented and deterministic.
- **P2.7 Prefix stability.** Editing only a suffix of the parent
  leaves the chunks (spans, bodies, and therefore `FactId`s) of the
  unchanged prefix intact.

### Slices 4–6 — indexes, fusion, collapse

- **P4.1 No ghost facts.** After a parent memory's delete or update,
  no fact keyed to a stale generation of that parent remains
  retrievable from any index.
- **P4.2 Rebuild idempotence.** Rebuilding derived state twice from
  the same git truth yields identical catalogs and identical index
  contents.
- **P6.1 Parent collapse.** After fusion and collapse, at most one
  fact per parent memory appears in a result list.
- **P6.2 Bounded overfetch.** Result limits are respected after
  collapse, and overfetch stays within its documented bound.

### Slice 7 — wire and telemetry

- **P7.1 Additive-only responses.** Chunk provenance only adds fields;
  no existing recall response field is renamed, removed, or re-typed.
- **P7.2 Content-free default telemetry.** Default telemetry rows
  carry UUID/`FactId`, ranks, match type, token/byte counts, latency,
  the truncation flag (`parent_would_truncate`), error class, and
  optionally a deployment-salted query fingerprint for repeat
  detection — never raw query, content, name, or heading.
  Content-bearing diagnostics exist only behind explicit operator
  opt-in with separate access controls, sampling, time-bounding,
  short retention, and a visible indicator that content capture is
  active. Rationale: the observability plane has different access and
  retention semantics than the store — it is shared, and trace
  retention can outlive memory deletion — plus cardinality cost.
  UUIDs are the ordinary investigation handle, resolved through
  memory-mcp's own authorization when content is needed; the opt-in
  path keeps semantic debugging possible without making the
  observability plane a shadow copy of the private layer by default.

## Deferred to later slices
- Slice 2: deterministic markdown chunker (tokenizer-budget
  coalescing, Unicode/code/list/long-tail tests).
- Slice 3: versioned derived catalog, atomic rebuild, staleness
  detection (schema/model/commit).
- Slices 4–5: chunk semantic index with parent-wide delete/update;
  fielded lexical index (body, heading path, parent name, tags).
- Slice 6: fact-level RRF, deterministic parent collapse, bounded
  overfetch.
- Slice 7: additive matched-chunk response wiring and
  content-free-by-default telemetry (P7.2: ids, ranks, counts, and
  flags by default; content-bearing diagnostics only behind gated,
  indicated operator opt-in).
- Slice 8: deterministic cross-reference extraction (populates
  `refs_out`; dangling references are valid).
- Slice 9: authorization-aware resolution/traversal — deferred until
  an authorization/lease boundary exists; denied must be
  indistinguishable from missing.

## Consequences
- Slices 2–7 implement against stable types instead of renegotiating
  identity mid-stream; index keys, telemetry, and feedback all share
  one addressing scheme.
- Chunker changes are honest breaking changes to derived state: a
  version bump rebuilds everything rather than mixing generations in
  one index.
- Deterministic ids make rebuilds idempotent and diffable — the same
  guarantee ADR-0039 relies on for repair convergence.
- The contract adds public API surface (`FactId`, `SourceSpan`,
  `ChunkerVersion`, and serde on `MemoryRef`) ahead of the behavior
  that uses it; until slice 7 wires responses, the types are
  load-bearing only for downstream slices. `FactRecord` and
  `MatchedChunk` stay `pub(crate)` until their owning slices (3 and 7)
  produce an external consumer — every `pub` item is a semver
  commitment, so the catalog and recall-wire shapes go public only
  when the wire actually carries them. This ADR remains the source of
  truth for that eventual public shape.
- `sha2` becomes a direct dependency (already present transitively;
  no new crates in the tree).
