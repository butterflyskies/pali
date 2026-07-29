# Pali documentation

The root [README](../README.md) explains why Pali exists and gets a new
user to a working server. This directory holds the detail needed after that
first success.

## Use Pali

- [Getting started](getting-started.md) — install, start the server, create a
  first memory, and configure git sync
- [Client setup](clients.md) — connect common MCP clients over Streamable HTTP
- [Tool reference](tools.md) — tool arguments, retrieval behavior, scopes, and
  feedback
- [Configuration](configuration.md) — server flags, environment variables,
  authentication, model settings, and health endpoints
- [Deployment](deployment.md) — run the container image or deploy to Kubernetes
- [Security](security.md) — trust boundaries, credentials, container hardening,
  and supply-chain controls

## Understand and change Pali

- [Contributing](../CONTRIBUTING.md) — development setup, required checks,
  design records, and pull request expectations
- [Architecture overview](architecture.md) — current storage, retrieval, sync,
  and operational boundaries
- [Architecture decision records](adr/) — binding design decisions and their
  trade-offs
- [Design artifacts](design/) — feature design documents produced before
  implementation
- [Roadmap](../ROADMAP.md) — open epics and completed work
- [Changelog](../CHANGELOG.md) — released changes

These Markdown files are arranged so they can become the source of a generated
documentation site without making that publishing machinery a prerequisite for
maintaining useful docs today.
