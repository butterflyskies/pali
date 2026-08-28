# ADR-0009: Configurable branch name, default main

## Status
Accepted

## Context
The memory repo's primary branch is assumed to be `main`, but some users or hosting
platforms may use a different default (e.g., `master`). Hardcoding the branch name
would force users to rename their branch or fork the code.

## Decision
Make the branch name configurable via `--branch` / `MEMORY_MCP_BRANCH`, defaulting
to `main`. Pass it through to push, pull, and any ref-based operations.

## Consequences
- Works out of the box for `main`-based repos.
- Users with `master` or custom branches set one flag.
- Branch name flows through push/pull refspecs — must be used consistently everywhere.

## Amendment (2026-08-27)

Pali 0.19.0 renames the environment-variable prefix without changing this
decision. Use `PALI_BRANCH`; the former variable name above remains as the
historical record.
