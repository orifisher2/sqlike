# sqlike — MCP server & clients

[![@sqlike/mcp](https://img.shields.io/npm/v/%40sqlike%2Fmcp?label=%40sqlike%2Fmcp&color=17a673)](https://www.npmjs.com/package/@sqlike/mcp)
[![@sqlike/cli](https://img.shields.io/npm/v/%40sqlike%2Fcli?label=%40sqlike%2Fcli&color=17a673)](https://www.npmjs.com/package/@sqlike/cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

Deterministic SQL **static analysis** and **query-equivalence** checking for Postgres, MySQL,
SQLite, and SQL Server — as an [MCP](https://modelcontextprotocol.io) server, a CLI, and a shared
client library. Part of [sqlike](https://sqlike.com).

These are **thin remote clients**. They tokenize your SQL **locally** — identifiers and literals are
masked before anything leaves your machine — and forward the tokenized query to the sqlike API for
analysis. They contain no analysis engine; that runs server-side and is closed.

## Install the MCP server

Add it to any MCP client (Claude Desktop, Cursor, etc.):

```json
{
  "mcpServers": {
    "sqlike": { "command": "npx", "args": ["-y", "@sqlike/mcp"] }
  }
}
```

Or install via [Smithery](https://smithery.ai/servers/orifisher2/sqlike). An optional
`SQLIKE_API_KEY` environment variable raises rate limits; without it you get the open anonymous tier.

## Tools

### `analyze`

Static analysis of one SQL query — validity, anti-patterns, suggested rewrites, and schema/index
advice. Returns the JSON analysis envelope.

| Argument    | Type    | Description                                                              |
| ----------- | ------- | ------------------------------------------------------------------------ |
| `sql`       | string  | The SQL query to analyze. **Required.**                                  |
| `schema`    | string  | Optional DDL (`CREATE TABLE` / `CREATE INDEX`) for column- & type-aware checks. |
| `dialect`   | string  | `postgres` (default), `mysql`, `sqlite`, or `mssql`.                     |
| `allow_raw` | boolean | Only used when a query fails to parse (so can't be tokenized): send raw SQL for a parse diagnostic. Default `false`. |

### `diff`

Check whether two SQL queries are **equivalent** (result-preserving) — for verifying a rewrite or
refactor. Returns a verdict (`Equivalent` / `EquivalentWithNotes` / `Differs` / `Undecided`), a
confidence level, and a per-property report (columns, rows, cardinality, order). `Undecided` never
means equivalent. This is a judgement an LLM cannot reliably self-grade.

| Argument  | Type   | Description                                                        |
| --------- | ------ | ----------------------------------------------------------------- |
| `sql_a`   | string | The original query. **Required.**                                 |
| `sql_b`   | string | The rewritten query to check against `sql_a`. **Required.**       |
| `schema`  | string | Optional DDL both queries resolve against (one shared schema).    |
| `dialect` | string | `postgres` (default), `mysql`, `sqlite`, or `mssql`.              |

## CLI

`crates/cli` builds `sqlike`, a command-line client:

```sh
sqlike check "SELECT * FROM users WHERE id = 1" --remote https://api.sqlike.com
```

## What's here

- **`crates/mcp`** — `sqlike-mcp`, the MCP server. Ships to npm as [`@sqlike/mcp`](packages/mcp).
- **`crates/cli`** — `sqlike`, the command-line client.
- **`crates/client`** — the shared, engine-free forwarder: tokenize → call API → detokenize.
- **`crates/core-parse`** — the SQL parser, stage model, tokenizer, and result types.
- **`packages/`** — the npm packaging for `@sqlike/mcp` (per-platform prebuilt binaries).

## Privacy

Tokenization happens here, on your machine, before any request. If a query can't be parsed it can't
be tokenized; the client then **refuses** to send it rather than transmit raw SQL, unless you
explicitly opt in (`allow_raw` / `--allow-raw`).

## Note

This repository is generated from the upstream monorepo (the source of truth). Please file issues
here; code changes are made upstream and mirrored.

## License

MIT OR Apache-2.0, at your option.
