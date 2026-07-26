# Pāli

Durable, local-first memory for AI agents.

> **Pāli** (Sanskrit/Prakrit): the sacred language in which the Buddhist canon
> was preserved — a language of durable, transmitted teachings.
> **páli** (Greek πάλι): *again* — read again, written again, reconstructed again.
> Named from **palimpsest**: a manuscript written over previous text, layers
> still visible through the revisions.
>
> Built with love by [butterflyskies](https://github.com/butterflyskies).

Pali gives any [Model Context Protocol](https://modelcontextprotocol.io/)
client a memory it can inspect, search, and carry between sessions. Memories stay
as Markdown in a git repository you control. Retrieval combines local semantic
embeddings with BM25 keyword search, so both concepts and exact phrases can find
their way back.

## Why Pali?

Agents forget the useful parts of yesterday: project decisions, debugging clues,
working preferences, and the reason a strange constraint exists. Pali turns
that context into a durable system instead of another prompt appendix.

- **Own the source of truth.** Memories are readable Markdown files with git
  history, not rows trapped in a hosted service.
- **Find meaning and wording.** Semantic and lexical rankings are fused, so a
  concept search and a buried exact phrase can both succeed.
- **Keep inference local.** Embeddings run on your machine with Candle and
  BGE-small-en-v1.5; no embedding API key is required.
- **Move between machines.** The memory repository can sync to a GitHub remote
  using built-in token-backed authentication.
- **Learn whether recall works.** Recall IDs and feedback tools turn retrieval
  quality into something you can measure.

## Quick start

Install and start the server:

```bash
cargo install pali
pali serve
```

The first run downloads the embedding model (about 130 MB) from Hugging Face.
Run `pali warmup` first if you want to populate the model cache ahead of
time.

Point an MCP client at the Streamable HTTP endpoint:

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

Client configuration formats differ. See the
[client setup guide](docs/clients.md) for ready-to-adapt examples.

Once connected, an agent can store a memory:

```json
{
  "name": "postgres/connection-pool-timeout",
  "content": "Under burst load, check whether checkout_timeout is lower than the slowest query.",
  "tags": ["postgres", "debugging"],
  "scope": "my-api"
}
```

Then retrieve it by meaning or by the words it contains:

```json
{
  "query": "database connection failures during traffic spikes",
  "scope": "my-api",
  "limit": 5
}
```

For installation from source, Docker, a first-run walkthrough, and git sync,
continue with [Getting started](docs/getting-started.md).

## What ships today

Pali exposes the following MCP tools:

| Job | Tools |
|---|---|
| Write and organize | `remember`, `edit`, `move`, `forget` |
| Find and inspect | `recall`, `read`, `list` |
| Synchronize | `sync` |
| Improve retrieval | `mark_applied`, `batch_mark_applied`, `recall_stats` |

`recall` runs semantic vector search and BM25 lexical search, then combines the
ranked lists with reciprocal rank fusion. Results say whether they matched via
`semantic`, `lexical`, or `both`. Exact phrases receive lexical precedence even
when they are buried in long memories.

Scopes are hierarchical namespaces such as `my-project` or
`org/team/project`. Querying a scope includes that namespace, its descendants,
and global memories. Omitting a scope searches global memories only; passing
`all` explicitly searches every scope. Scopes organize retrieval. They are not
an access-control boundary.

See the [tool reference](docs/tools.md) for arguments, result contracts, scope
behavior, and recall feedback.

## How it fits together

```text
Agent ── Streamable HTTP ──▶ pāli
                                  │
                    ┌─────────────┼─────────────┐
                    ▼             ▼             ▼
               Markdown + git   BM25          local embeddings
               source of truth  lexical index HNSW vector index
                    │
                    ▼
                git remote
```

The Markdown repository is authoritative. Search indexes are derived from it and
can be rebuilt. Memory content is embedded locally; it leaves the machine only
when you deliberately sync the git repository to a remote.

Significant choices and their trade-offs live in the
[architecture overview](docs/architecture.md) and
[architecture decision records](docs/adr/), including
[hybrid retrieval](docs/adr/0038-bm25-hybrid-retrieval.md).

## Documentation

- [Getting started](docs/getting-started.md) — install, run, connect, and sync
- [Client setup](docs/clients.md) — MCP configuration examples
- [Tool reference](docs/tools.md) — tool and retrieval contracts
- [Configuration](docs/configuration.md) — server flags, environment variables,
  authentication, and health endpoints
- [Deployment](docs/deployment.md) — containers and Kubernetes
- [Security](docs/security.md) — trust boundaries, credential handling,
  hardened deployment, and supply-chain controls
- [Architecture](docs/architecture.md) — storage, retrieval, sync, and
  operational boundaries
- [Documentation map](docs/README.md) — guides, architecture, and contributor
  references
- [Contributing](CONTRIBUTING.md) — development setup, checks, design records,
  and pull requests
- [Roadmap](ROADMAP.md) — planned work and open epics

## Contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, required
checks, architecture/design expectations, and pull request guidance.

The core verification loop is:

```bash
cargo nextest run --workspace --no-fail-fast
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo doc --no-deps
```

Repository conventions, required checks, and the review loop are documented in
[CONTRIBUTING.md](CONTRIBUTING.md). Agent-specific workspace instructions live
separately in [AGENTS.md](AGENTS.md).

## License

Licensed under either

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
