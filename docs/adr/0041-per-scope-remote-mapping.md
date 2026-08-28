# ADR-0041: Per-scope remote mapping for sync

## Status
Accepted

## Context
memory-mcp uses a single git repo (`~/.memory-mcp`) with a single `origin` remote. All scopes share the same remote. This prevents constructs from isolating proprietary data: a construct working with NDA-covered code needs memories in certain scopes to sync to a private repo, while personal and shared memories continue syncing to the existing shared repos.

Two memory-mcp instances would technically work but destroy the single-recall UX — constructs must remember which server to query, need two recall calls at boot, and face decision fatigue on every save. Behavioral discipline ("just don't write secrets to shared scopes") is structure-over-willpower in reverse.

## Decision
Support multiple independent git repos, one per configured scope mapping, with a unified read layer. A TOML config file (`~/.config/memory-mcp/config.toml` or `MEMORY_MCP_CONFIG`) defines scope-to-repo mappings:

```toml
[[remotes]]
scope = "work"
url = "git@github.com:private-org/memory-work.git"
path = "~/.memory-mcp-work"
```

A new `RepoRouter` type holds a default `MemoryRepo` (the existing single-repo path) plus zero or more scope-mapped repos. Write operations (`save`, `delete`, `move`, `edit`) route to the repo that owns the scope. Read operations (`list`, `recall`) aggregate across all repos. `sync` pushes/pulls each repo independently.

When no config file exists, behavior is identical to today: one repo, one remote.

### Alternatives considered
- **Single repo, multiple remotes with selective push:** Git doesn't natively support pushing a directory subset to a specific remote. Would require git-filter-branch or subtree splits — fragile and complex.
- **Git submodules per scope:** Adds operational complexity (submodule init, recursive operations) for a use case that doesn't need shared history across scopes.

## Consequences
- Config file is optional — no breaking change for existing deployments
- Each scope-mapped repo gets its own vector index subdirectory
- `move` across repo boundaries requires delete-from-source + create-in-destination (not a single git commit)
- Auth token must have access to all configured remotes
- Health reporting aggregates across repos — any repo failure degrades the subsystem

## Amendment (2026-08-27)

Pali 0.19.0 renames the environment-variable prefix without changing the
configuration-file design. Use `PALI_CONFIG`; the former variable name above
remains as the historical record.
