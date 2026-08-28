# ADR-0004: No Tokens in CLI Arguments

## Status
Accepted

## Context
The server needs a GitHub token for git push/pull to the remote. Tokens could be provided
via CLI flag, environment variable, or stored file.

## Decision
No CLI flag for tokens. Resolution order:
1. `MEMORY_MCP_GITHUB_TOKEN` environment variable (k8s: Secret mounted as env var)
2. Stored token file (`~/.memory-mcp/token`, written by device auth flow)
3. GitHub OAuth device auth flow (interactive, stores result to #2)

## Consequences
- Tokens never appear in `ps aux` or `/proc/<pid>/cmdline` output
- k8s deployment uses Secret → env var mount (standard pattern)
- First run on a new machine triggers interactive device flow, then stores token for reuse
- No dependency on `gh` CLI or git's credential helper being configured

## Amendment (2026-08-27)

Pali 0.19.0 renames the environment-variable prefix without changing this
resolution order or the no-token-arguments decision. Use `PALI_GITHUB_TOKEN`;
the former variable name above remains as the historical record.
