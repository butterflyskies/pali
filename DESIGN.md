# Pāli

A semantic memory system for AI coding agents, exposed as an MCP server. Memories are stored as files in a git repository, synced across devices via a private GitHub remote, and indexed for semantic retrieval.

## Problem

Current memory systems (e.g. Serena memories, Claude Code auto-memory) have limitations:

- **No sync** — memories are local files tied to a single machine
- **No semantic search** — retrieval requires knowing exact memory names; you must list all memories then pick by name
- **Coupled to other tools** — memory lifecycle is tied to the coding tool's MCP server
- **No scalable organization** — flat namespace, manual categorization, no way to surface relevant context automatically

## Core idea

Separate memory from coding tools. Build a dedicated MCP server that:

1. Stores memories as files in a **git repository** (the "memory repo")
2. Syncs across devices via **push/pull to a private GitHub repo**
3. Maintains a **local embedding index** for semantic search, rebuilt on pull
4. Exposes high-level operations — `remember`, `recall`, `forget` — rather than low-level file ops

## Key design shift

Instead of:
```
list_memories → scan names → read_memory("exact-name")
```

The interface becomes:
```
recall("what do I know about Rust error handling patterns?")
```

The agent describes what it needs, and the system returns relevant memories ranked by semantic similarity. No need to enumerate or guess filenames.

## Architecture

```
┌─────────────┐     ┌──────────────────┐     ┌─────────────┐
│  AI Agent    │────▶│  pāli            │────▶│  Memory Repo│
│  (Claude)    │◀────│  (MCP server)    │◀────│  (git)      │
└─────────────┘     └──────────────────┘     └──────┬──────┘
                            │                        │
                            ▼                        ▼
                    ┌──────────────┐         ┌──────────────┐
                    │  Embedding   │         │  GitHub      │
                    │  Index       │         │  (private)   │
                    │  (local)     │         │  remote      │
                    └──────────────┘         └──────────────┘
```

### Memory repo

- A regular git repository, separate from any project
- Each memory is a markdown file (or other plain text)
- Directory structure provides coarse organization (e.g. `global/`, `projects/cc-toolgate/`)
- Commits track memory evolution over time
- Push/pull to a private GitHub repo for cross-device sync

### Embedding index

- Built locally from the memory files
- Rebuilt on pull (or incrementally updated)
- Used for semantic search — find memories by meaning, not by name
- Could use a local embedding model or an API-based one
- Stored in the repo (or gitignored and rebuilt) — TBD

### MCP server

Exposes tools to the AI agent:

- **`remember(content, [tags], [scope])`** — store a new memory or update an existing one. The server decides the filename/path, writes the file, commits, and updates the index.
- **`recall(query, [scope], [limit])`** — semantic search across memories. Returns ranked results with content and metadata.
- **`forget(query_or_id)`** — remove a memory. Commits the deletion.
- **`sync()`** — pull from remote, rebuild index, push local changes. Could also run automatically on server start and periodically.

## Open questions

- **Embedding model**: local (e.g. sentence-transformers) vs API (e.g. Anthropic/OpenAI embeddings)? Local is simpler for offline use and privacy. API may give better quality.
- **Conflict resolution**: what happens when two devices edit the same memory? Git merge handles text conflicts, but semantic conflicts (contradictory facts) need a strategy.
- **Memory format**: plain markdown? Frontmatter with metadata (tags, timestamps, source)? Structured YAML?
- **Deduplication**: how to detect when a new `remember` call is updating an existing memory vs creating a new one? Semantic similarity threshold?
- **Scope model**: global memories vs project-scoped vs session-scoped? How does scoping map to directory structure?
- **Index storage**: commit the index to the repo (portable but larger), or gitignore it and rebuild (clean but slower on first pull)?
- **Language/runtime**: Python (rich embedding ecosystem, uvx-friendly) vs Rust (performance, aligns with existing projects)?
- **Auth for sync**: SSH keys? GitHub CLI token? How to bootstrap on a new device?

## Prior art / related

- Serena memories (file-based, no sync, no semantic search)
- Claude Code auto-memory (flat markdown, single machine)
- mem0 / memgpt (agent memory frameworks, but SaaS-oriented)
- Obsidian + git sync (similar git-backed knowledge base pattern)
