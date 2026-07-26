# Contributing to Pāli

This project is a Rust MCP server with git-backed durable state and local
retrieval indexes. Changes to storage, retrieval, wire contracts, or startup
behavior can affect existing memory repositories, so keep proposals small,
testable, and explicit about compatibility.

## Set up a development checkout

Pāli declares Rust 1.95 as its minimum supported version.

```bash
git clone https://github.com/butterflyskies/pali.git
cd pali
git config core.hooksPath .githooks
cargo check
```

The repository uses [cargo-nextest](https://nexte.st/) for its full test suite.
Some dependencies compile native code, so a C/C++ toolchain, CMake,
`pkg-config`, and D-Bus development headers may be required on Linux.

## Before writing code

- Read the [architecture decision records](docs/adr/) before starting, then
  revisit the records that govern your change. They are binding unless a new
  decision explicitly supersedes one.
- Search existing issues and pull requests before opening another lane for the
  same problem.
- Keep one coherent concern per change. Separate dependent concerns into
  stacked pull requests rather than mixing unrelated work.
- Preserve public APIs, on-disk memory formats, MCP response contracts, and
  existing repository data unless the change intentionally includes a
  compatible migration or versioned break.

For a feature, migration, concurrency change, or other risky work, document the
problem, requirements, architecture, risks, and test plan under `docs/design/`
before implementation. Scale the artifact to the change; a local, reversible
fix does not need a five-document ceremony.

Write a new ADR when alternatives were seriously weighed or the change commits
the project to an architectural pattern, dependency, security model, or public
boundary. Follow the existing sequential naming and capture the decision,
alternatives, consequences, and rationale.

## Implement and test

- Add behavioral tests with the change. Prefer in-process Tower tests over
  spawning the server unless the binary's startup behavior is the subject.
- Use `#[tokio::test(start_paused = true)]` for time-dependent behavior.
- Avoid panics in production code; propagate domain errors through
  `MemoryError`.
- Keep public API additions deliberate and documented. `pub(crate)` is the
  default for implementation details.
- Update user documentation and `CHANGELOG.md` when behavior visible to users
  changes.

Run the repository gates before requesting review:

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo doc --no-deps
cargo nextest run --workspace --no-fail-fast
cargo check --features k8s
```

Before a version bump, also run:

```bash
cargo nextest run --workspace --no-fail-fast --all-features
cargo semver-checks check-release
```

The configured pre-commit hook runs formatting, clippy, rustdoc, and a staged
secret scan. It complements the full test and feature checks; it does not
replace them.

## Open a pull request

Explain the user-visible problem, the chosen behavior, and how the tests prove
it. Link the issue or design artifact when one exists. Call out compatibility,
failure recovery, operational impact, and follow-up work instead of leaving
those decisions only in review comments.

Do not merge or publish a release yourself. The maintainer handles merges and
releases after review and required checks converge.

Agent-specific workspace and review-loop instructions live in
[AGENTS.md](AGENTS.md). Human contributors can use this document as the public
entrypoint.
