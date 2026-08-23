# Configuration

Server options are available as command-line flags and environment variables.
Run `pali serve --help` against your installed version for the complete,
authoritative list.

## Server

| Flag | Environment variable | Default | Purpose |
|---|---|---|---|
| `--bind` | `MEMORY_MCP_BIND` | `127.0.0.1:8080` | HTTP listener address |
| `--repo-path` | `MEMORY_MCP_REPO_PATH` | `~/.memory-mcp` | Git-backed memory repository |
| `--config` | `MEMORY_MCP_CONFIG` | `~/.config/memory-mcp/config.toml` | TOML config file for remote mapping and sibling-store topology; empty string disables config loading |
| `--mcp-path` | `MEMORY_MCP_PATH` | `/mcp` | Streamable HTTP MCP path |
| `--remote-url` | `MEMORY_MCP_REMOTE_URL` | unset | Git remote; omit for local-only mode |
| `--branch` | `MEMORY_MCP_BRANCH` | `main` | Branch used for push and pull |
| `--allowed-host` | `MEMORY_MCP_ALLOWED_HOST` | none | Additional accepted HTTP Host value; repeatable |
| `--require-remote-sync` | `MEMORY_MCP_REQUIRE_REMOTE_SYNC` | `false` | Make remote sync health affect readiness |
| `--recall-log-busy-timeout` | `MEMORY_MCP_RECALL_LOG_BUSY_TIMEOUT` | `5` | SQLite lock wait in seconds |
| `--health-stale-secs` | `MEMORY_MCP_HEALTH_STALE_SECS` | `0` | Mark inactive subsystems stale; 0 disables |

## Sessions and embedding work

| Flag | Environment variable | Default | Purpose |
|---|---|---|---|
| `--max-sessions` | `MEMORY_MCP_MAX_SESSIONS` | `100` | Maximum concurrent MCP sessions |
| `--session-rate-limit` | `MEMORY_MCP_SESSION_RATE_LIMIT` | `10` | New sessions per rate window; 0 disables |
| `--session-rate-window-secs` | `MEMORY_MCP_SESSION_RATE_WINDOW_SECS` | `60` | Session rate-limit window |
| `--idle-timeout-secs` | `MEMORY_MCP_IDLE_TIMEOUT_SECS` | `14400` | Session idle timeout; 0 disables |
| `--max-session-lifetime-secs` | `MEMORY_MCP_MAX_SESSION_LIFETIME_SECS` | `0` | Absolute session lifetime; 0 disables |
| `--embed-timeout-secs` | `MEMORY_MCP_EMBED_TIMEOUT_SECS` | `30` | Maximum time for one embedding call |
| `--embed-queue-size` | `MEMORY_MCP_EMBED_QUEUE_SIZE` | `64` | Bounded embedding-worker queue |

Builds with the `otlp` feature also expose `--otlp-required` and
`--otlp-optional` (and matching `MEMORY_MCP_*` variables) to select strict or
best-effort OTLP span export.

## Authentication

Authenticate to a private GitHub remote with the OAuth device flow:

```bash
pali auth login
pali auth status
```

Select storage explicitly when needed:

```bash
pali auth login --store keyring
pali auth login --store file
pali auth login --store stdout
```

Builds with the `k8s` feature also support `--store k8s-secret`.

At runtime, token resolution checks `MEMORY_MCP_GITHUB_TOKEN`, then the token
file, then the system keyring. Tokens are never accepted as CLI arguments.

This authentication config is for outbound Git synchronization. Federated
memory reads do not use it and never borrow a server credential.

## Federated sibling stores

Name this Pali instance and its trusted sibling MCP endpoints in the same TOML
file used for per-scope remotes:

```toml
store_id = "personal"
straddle_timeout_ms = 2000

[siblings]
fcc = "https://friends-collective-conscious.svc.echoes/mcp"
lcc = "https://collective-conscious.svc.echoes/mcp"
```

Then call `read` with `straddle: true` to query the local store and all
siblings concurrently. The response keeps each fragment under its `store_id`
and distinguishes `not_found`, `unreachable`, and `identity_unavailable`.
Ordinary `read` calls retain their existing response shape.

Sibling endpoints are topology, not authority. They must use HTTPS (except
loopback HTTP for tests) and cannot contain credentials, query parameters, or
fragments. A straddled request forwards the caller's inbound bearer token
opaquely to each configured sibling. Without that caller delegation, Pali
returns the local result, marks each sibling `identity_unavailable`, and makes
no sibling requests.

## Embedding model

Pali computes embeddings locally with Candle and BGE-small-en-v1.5
(384 dimensions). Model files use the Hugging Face cache under `HF_HOME`.
`pali warmup` downloads them without starting the server.

A dedicated worker thread owns the embedding engine. The queue is bounded, and
a timed-out request does not permanently wedge the worker.

## Health endpoints

| Endpoint | Contract |
|---|---|
| `GET /healthz` | Liveness; returns 200 while the HTTP process is serving. |
| `GET /readyz` | Readiness; returns 200 or 503 with passive subsystem status. |
| `GET /version` | Build and version information. |

`/readyz` reports git, embedding, vector-index, and optional sync health without
performing I/O inside the probe handler. See [Deployment](deployment.md) for
Kubernetes manifests and operational guidance.
