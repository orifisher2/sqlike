# @sqlike/mcp

MCP server for [sqlike](https://sqlike.com), a deterministic SQL static analyzer and advisor
(validity, anti-patterns, rewrites, index and schema advice) and query-equivalence checker for
Postgres, MySQL, MariaDB, SQLite, and SQL Server.

Point your coding agent at it and the SQL it writes gets checked before it runs, by something with
no model in the loop and no opinion to guess with.

This package is a **thin remote client**. It tokenizes your SQL locally, so identifiers and literals
are masked before anything leaves your machine, and forwards only the tokenized query to the sqlike
API. There is no analysis engine in it.

## Use it

No install needed. Point your MCP client at it via `npx`:

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

- **`analyze`**: static analysis of one query. Takes `sql`, plus optional `schema` DDL for column
  and type aware checks and a `dialect` of `postgres` (default), `mysql`, `mariadb`, `sqlite`, or
  `mssql`. Returns the JSON analysis envelope.
- **`diff`**: whether two queries are equivalent. Takes `sql_a` and `sql_b`, plus the same optional
  `schema` and `dialect`. Returns a verdict (`Equivalent`, `EquivalentWithNotes`, `Differs`, or
  `Undecided`), a confidence level, and a report per property (columns, rows, cardinality, order).
  `Undecided` never means equivalent.

A query that cannot be parsed cannot be tokenized, so the tool refuses rather than send raw SQL.
Overriding that is an explicit `allow_raw: true` on `analyze`.

### Configuration

Both are read from the environment:

- `SQLIKE_URL`: API base URL (default `https://api.sqlike.com`).
- `SQLIKE_API_KEY`: optional, sent as a Bearer token for higher rate limits.

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
package (`@sqlike/mcp-linux-x64`, `@sqlike/mcp-darwin-arm64`, and so on), so npm installs only the
one matching your OS and CPU. No install scripts run, which means this works with `--ignore-scripts`
and in locked-down environments. Supported: linux x64 and arm64, macOS x64 and arm64, Windows x64.

## License

MIT OR Apache-2.0
