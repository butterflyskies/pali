# Getting started

This guide takes Pali from installation to a first successful recall.

## Install

From crates.io:

```bash
cargo install pali
```

Or build the current source:

```bash
git clone https://github.com/butterflyskies/pali.git
cd pali
cargo install --path .
```

## Start the server

```bash
pali serve
```

By default, Pali listens on `127.0.0.1:8080`, exposes MCP at
`/mcp`, and stores memories in `~/.memory-mcp`.

The first run downloads BGE-small-en-v1.5 (about 130 MB) from Hugging Face.
Pre-download it when preparing an offline or reproducible environment:

```bash
pali warmup
```

To change the bind address or repository location:

```bash
PALI_BIND=0.0.0.0:9090 \
PALI_REPO_PATH=/path/to/memories \
pali serve
```

See [Configuration](configuration.md) for the complete runtime surface.

## Connect an MCP client

Configure the client to use the Streamable HTTP endpoint
`http://localhost:8080/mcp`. For example, clients using the common
`mcpServers` shape accept:

```json
{
  "mcpServers": {
    "memory": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

See [Client setup](clients.md) for client-specific locations and field names.

## Store and retrieve a memory

Ask the connected agent to call `remember`:

```json
{
  "name": "postgres/connection-pool-timeout",
  "content": "Under burst load, check whether checkout_timeout is lower than the slowest query.",
  "tags": ["postgres", "debugging"],
  "scope": "my-api"
}
```

Then ask it to call `recall`:

```json
{
  "query": "database connection failures during traffic spikes",
  "scope": "my-api",
  "limit": 5
}
```

`recall` combines semantic and keyword rankings. Each result reports its
`match_type`, a content snippet, and whether the snippet was truncated. Use
`read` before acting on a truncated result.

## Run with Docker

The published image contains the embedding model, so startup does not need to
download it:

```bash
docker run -d --name pali \
  -p 8080:8080 \
  -v "$HOME/.memory-mcp:/data/repo" \
  ghcr.io/butterflyskies/memory-mcp:latest
```

The volume is required for persistence. Without it, memories disappear with
the container.

## Configure git sync

Pali works without a remote. To share the repository across machines,
configure a remote URL when starting the server:

```bash
PALI_REMOTE_URL=https://github.com/you/my-memories.git \
pali serve
```

Authenticate first when the remote requires a GitHub token:

```bash
pali auth login
```

The connected agent can then call the `sync` MCP tool. There is no standalone
`pali sync` CLI subcommand.

For private remotes, token storage, containers, and Kubernetes, continue with
[Configuration](configuration.md) and [Deployment](deployment.md).
