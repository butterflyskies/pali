# Client setup

Pali exposes MCP over Streamable HTTP. The default endpoint is
`http://localhost:8080/mcp`.

Client configuration formats evolve independently from Pali. Treat these
examples as starting points and consult the client documentation when a current
release rejects a field or location.

## Claude Code

Add to `~/.claude.json` or a project's `.mcp.json`:

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

## Cursor

Add to `.cursor/mcp.json` for a project or `~/.cursor/mcp.json` globally:

```json
{
  "mcpServers": {
    "memory": {
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

## Visual Studio Code with GitHub Copilot

Add to `.vscode/mcp.json`:

```json
{
  "servers": {
    "memory": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

Visual Studio Code uses `servers` as the root key rather than `mcpServers`.

## Windsurf

Add to `~/.codeium/windsurf/mcp_config.json`:

```json
{
  "mcpServers": {
    "memory": {
      "serverUrl": "http://localhost:8080/mcp"
    }
  }
}
```

## Continue

Add `.continue/mcpServers/memory.yaml`:

```yaml
mcpServers:
  - name: memory
    type: streamable-http
    url: http://localhost:8080/mcp
```

## Clients that require stdio

Pali intentionally ships only Streamable HTTP. A client without native
HTTP support can use a bridge such as `mcp-remote`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "npx",
      "args": ["mcp-remote", "http://localhost:8080/mcp"]
    }
  }
}
```

The bridge is a separate process and dependency; it is not part of Pali.
