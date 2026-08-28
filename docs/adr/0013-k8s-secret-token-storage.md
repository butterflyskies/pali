# ADR-0013: Kubernetes Secret token storage

## Status
Accepted

## Context
memory-mcp needs to store GitHub tokens in Kubernetes for server deployments. The token
must end up in a K8s Secret so the pod spec can mount it as `MEMORY_MCP_GITHUB_TOKEN`.

## Decision
Add `--store k8s-secret` to `auth login`, feature-gated behind a `k8s` cargo feature.
Uses the `kube` crate to create/update an Opaque Secret with a single `token` key.
The k8s store is write-only — token resolution uses the existing env var chain (pod
mounts the Secret as an env var). This avoids pulling kube deps into the server's hot path.

Namespace and secret name are configurable via `--k8s-namespace` (default: `memory-mcp`)
and `--k8s-secret-name` (default: `memory-mcp-github-token`). The data key `token` is
hardcoded to prevent drift with the pod spec.

## Consequences
- Desktop builds are unaffected (feature is opt-in)
- `cargo build --features k8s` pulls kube + k8s-openapi (large deps, compile time increase)
- Re-running `auth login --store k8s-secret` overwrites the Secret (get-then-replace)
- RBAC for secrets in the target namespace is required

## Amendment (2026-08-27)

Pali 0.19.0 renames the pod environment variable to `PALI_GITHUB_TOKEN`.
Secret storage, the `token` data key, and the write-only Kubernetes flow are
unchanged; the former variable name above remains as the historical record.
