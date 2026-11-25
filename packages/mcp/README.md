# @sqlike/mcp

MCP server for [sqlike](https://sqlike.com), a deterministic SQL static analyzer and advisor
(validity, anti-patterns, rewrites, index and schema advice) and query-equivalence checker for
Postgres, MySQL, MariaDB, SQLite, SQL Server, and DuckDB.

It gives a coding agent two tools, to check the SQL it writes and to check that a rewrite returns
the same results. No language model takes part in the analysis.

This package is a remote client. It tokenizes your SQL on your machine, which replaces identifiers
and literals with placeholders, and sends only the tokenized query to the sqlike API. There is no
analysis engine in it.

## Use it

There is nothing to install. Add it to your MCP client with `npx`:

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

It provides two tools:

- `analyze`: static analysis of one query. Takes `sql`, and optionally `schema` DDL for column and
  type checks, `stats` (row counts per table), `explain` (the query plan), and a `dialect` of
  `postgres` (default), `mysql`, `mariadb`, `sqlite`, `mssql`, or `duckdb`. Returns the analysis
  as JSON.
- `diff`: whether two queries return the same results. Takes `sql_a` and `sql_b`, the same optional
  `schema` and `dialect`, and `columns` (`by_position` by default, or `by_name` for code that reads
  columns by name). Returns a verdict (`Equivalent`, `EquivalentWithNotes`, `Differs`, `Undecided`,
  or `NotComparable`), a confidence level, and a report per property (columns, rows, cardinality,
  order). `Undecided` does not mean equivalent.

A query that does not parse is answered on your machine and nothing is sent. A query that holds
text the tokenizer cannot replace is refused, and nothing is sent: the MCP server never sends a
query unmasked. To analyze such a query as is, run `sqlike check --allow-raw` yourself.

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
      "env": { "SQLIKE_API_KEY": "vq_..." }
    }
  }
}
```

## How it ships

The server is a native binary. Each platform's binary is published as its own optional-dependency
package (`@sqlike/mcp-linux-x64`, `@sqlike/mcp-darwin-arm64`, and so on), so npm installs only the
one matching your OS and CPU. No install scripts run, so it works with `--ignore-scripts`. Supported: linux x64 and arm64, macOS x64 and arm64, Windows x64.

## License

Apache-2.0. See the `LICENSE` file in the package.
