# @sqlike/mcp

MCP server for [sqlike](https://sqlike.com) — a deterministic SQL static analyzer and advisor
(validity, anti-patterns, rewrites, schema advice) for Postgres, MySQL, SQLite, and SQL Server.

This package is a **thin remote client**: it tokenizes your SQL locally (identifiers and literals
are replaced before anything leaves your machine) and forwards the tokenized query to the sqlike
API for analysis. It contains no analysis engine.

## Use it

No install needed — point your MCP client at it via `npx`:

```json
{
  "mcpServers": {
    "sqlike": {
      "command": "npx",
      "args": ["-y", "@sqlike/mcp"]
    }
  }
}
```

It exposes two tools:

- **`analyze`** — static analysis of a single SQL query.
- **`diff`** — whether two queries are equivalent.

### Configuration

Both are read from the environment:

- `SQLIKE_URL` — API base URL (default `https://api.sqlike.com`).
- `SQLIKE_API_KEY` — optional; sent as a Bearer token for higher rate limits.

```json
{
  "mcpServers": {
    "sqlike": {
      "command": "npx",
      "args": ["-y", "@sqlike/mcp"],
      "env": { "SQLIKE_API_KEY": "sk_..." }
    }
  }
}
```

## How it ships

The server is a native binary. Each platform's binary is published as its own optional-dependency
package (`@sqlike/mcp-linux-x64`, `@sqlike/mcp-darwin-arm64`, …); npm installs only the one matching
your OS and CPU. No install scripts run — this works with `--ignore-scripts` and in locked-down
environments. Supported: linux x64/arm64, macOS x64/arm64, Windows x64.

## License

MIT OR Apache-2.0
