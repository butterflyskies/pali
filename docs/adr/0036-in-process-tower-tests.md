# ADR-0036: In-process tower tests over subprocess integration tests

## Status
Accepted

## Context
Integration tests in `tests/allowed_hosts.rs`, `tests/readyz.rs`, and `tests/auth.rs` spawned
the full binary, allocated ports, and polled `/healthz` with a 10s timeout. These flaked on CI
due to cold-start latency under parallel test load — test durations ranged from ~10s to
intermittent 10s timeout failures.

## Decision
Replace subprocess integration tests with in-process tests using `tower::ServiceExt::oneshot()`
and stub backends (`StubEmbeddingBackend`, `InMemoryStore`). Shared test infrastructure lives
in `tests/common/mod.rs` (`build_stub_state()`, `build_test_router()`). Subprocess tests that
verify clap argument parsing were converted to unit tests in `src/main.rs`.

## Consequences
- Test execution dropped from ~10s per test to ~18-26ms (500x faster)
- No port allocation, no process spawning, no polling — deterministic execution
- Stubs are explicit about what they fake — no hidden behavior from full server startup
- `StubEmbeddingBackend` is a deterministic, offline hash embedder (FNV-1a bag of tokens), so
  stub-backed tests can populate the store; `build_state_with_embedding()` substitutes a
  deliberately broken backend for control tests (#377)
- Shared `tests/common/mod.rs` establishes a pattern for future integration tests
- Subprocess tests are still appropriate when testing the binary's actual startup behavior

## References
- #227
