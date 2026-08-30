# sqlike: MCP server and CLI

[![@sqlike/mcp](https://img.shields.io/npm/v/%40sqlike%2Fmcp?label=%40sqlike%2Fmcp&color=17a673)](https://www.npmjs.com/package/@sqlike/mcp)
[![@sqlike/cli](https://img.shields.io/npm/v/%40sqlike%2Fcli?label=%40sqlike%2Fcli&color=17a673)](https://www.npmjs.com/package/@sqlike/cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**Check your SQL before it runs, and check whether a rewrite still returns the same results.**

[sqlike](https://sqlike.com) reads a query and tells you what is wrong with it: validity errors,
anti-patterns, rewrites it can apply for you, and index advice. It also compares two queries and
reports whether the second one still returns the same results. There is no model anywhere in the
analysis, so the same query always gets the same answer.

This repository holds the clients: an [MCP](https://modelcontextprotocol.io) server, a CLI, and the
library they share. They tokenize your SQL locally, so identifiers and literals are masked before
anything leaves your machine, and forward only the tokenized query. The analysis engine is not in
this repo. It runs server-side and is closed.

**Dialects:** Postgres, MySQL, MariaDB, SQLite, SQL Server, and DuckDB. Each rule carries a verdict
measured on that engine, so the severity you get is that database's behaviour and not Postgres by
inheritance. DuckDB is the columnar one: it has no general-purpose secondary index, so the ten index
advisors are replaced there by four columnar ones.

## Why

A lot of SQL is written by an AI now, and the SQL it writes reads well more often than it runs
correctly. A `LEFT JOIN` turns into an `INNER` and rows quietly disappear. A `WHERE` goes missing and
the `UPDATE` hits every row. Two tables get joined on the wrong key. None of it looks wrong on the
page, and none of it shows up until it has already done something.

sqlike is the check in between. It flags unsafe patterns from a catalog of over 160 rules, each one
verified against a real database before it ships, and it decides whether a rewrite preserves results.
That second check is sound rather than complete: it certifies the rewrites it can prove, and when it
cannot prove one it answers `Undecided` instead of guessing. `Undecided` never means equivalent.

Because nothing is generated, there is no retry loop, no per-token cost, and no variance between two
runs on the same input. The equivalence check normalizes rather than solves, so it answers in about
a millisecond where the state-of-the-art academic prover takes hundreds. That is
[measured head to head](https://sqlike.com/benchmark), including the queries where the prover wins.

## Install the MCP server

Add it to any MCP client (Claude Code, Claude Desktop, Cursor, and so on):

```json
{
  "mcpServers": {
    "sqlike": { "command": "npx", "args": ["-y", "@sqlike/mcp"] }
  }
}
```

Or install it through [Smithery](https://smithery.ai/server/orifisher2/sqlike). Set `SQLIKE_API_KEY`
if you have a key and want the higher rate limits. Without one you get the anonymous tier, which
needs no signup.

## Tools

### `analyze`

Static analysis of one query: validity, anti-patterns, suggested rewrites, and schema and index
advice. Returns the JSON analysis envelope.

| Argument    | Type    | Description                                                              |
| ----------- | ------- | ------------------------------------------------------------------------ |
| `sql`       | string  | The query to analyze. **Required.**                                      |
| `schema`    | string  | Optional DDL (`CREATE TABLE` / `CREATE INDEX`) for column and type aware checks. |
| `dialect`   | string  | `postgres` (default), `mysql`, `mariadb`, `sqlite`, `mssql`, or `duckdb`. |
| `allow_raw` | boolean | Only used when a query fails to parse, and so cannot be tokenized: send the raw SQL to get a parse diagnostic. Default `false`. |

### `diff`

Checks whether two queries are equivalent, which is the judgement an LLM cannot reliably make about
its own rewrite. Returns a verdict (`Equivalent`, `EquivalentWithNotes`, `Differs`, or `Undecided`),
a confidence level, and a report per property (columns, rows, cardinality, order), so you see what
changed rather than a single yes or no.

| Argument  | Type   | Description                                                        |
| --------- | ------ | ----------------------------------------------------------------- |
| `sql_a`   | string | The original query. **Required.**                                  |
| `sql_b`   | string | The rewritten query to check against `sql_a`. **Required.**        |
| `schema`  | string | Optional DDL both queries resolve against (one shared schema).     |
| `dialect` | string | `postgres` (default), `mysql`, `mariadb`, `sqlite`, `mssql`, `duckdb`. |

## CLI

The same checks from a terminal or a CI job. Run it with no install, or put it on the path:

```sh
npx -y @sqlike/cli --help
npm i -g @sqlike/cli
brew install orifisher2/sqlike/sqlike
```

```sh
# analyze a query
sqlike check query.sql --remote https://api.sqlike.com

# with a schema, reading from stdin, machine-readable output
cat query.sql | sqlike check - --schema schema.sql --json --remote https://api.sqlike.com

# check that a rewrite is equivalent (this one always runs server-side)
sqlike diff before.sql after.sql
```

If you have the query's `EXPLAIN` output, pass it with `--explain plan.json` and the real access
paths will confirm or dismiss the index findings. The plan is tokenized before it leaves the machine,
same as the query and the schema.

`check` exits 0 when the query is clean, 1 on advisories, and 2 when something blocks. `diff` exits
0 for equivalent, 1 for differs, and 2 for undecided. Both use 3 for an operational failure, so a
pipeline can tell "the query is bad" from "the call did not go through".

## Private by design

Tokenization happens here, on your machine, before any request goes out. sqlike never sees your real
table names, columns, or values, so there is nothing to leak and nothing to train on. An AI
assistant needs the real thing to help you; sqlike does not.

If a query cannot be parsed it cannot be tokenized, and the client refuses to send it rather than
transmit raw SQL. Sending it anyway is an explicit opt-in (`allow_raw` on the tools, `--allow-raw` on
the CLI).

See [THREAT-MODEL.md](THREAT-MODEL.md) for what that does and does not cover.

## What is in this repo

- **`crates/mcp`**: `sqlike-mcp`, the MCP server. Ships to npm as [`@sqlike/mcp`](packages/mcp).
- **`crates/cli`**: `sqlike`, the command-line client. Ships to npm as [`@sqlike/cli`](packages/cli).
- **`crates/client`**: the shared forwarder, with no engine in it: tokenize, call the API, detokenize.
- **`crates/core-parse`**: the SQL parser, stage model, tokenizer, and result types.
- **`packages/`**: the npm packaging, with a prebuilt binary per platform.
- **`skills/`**: the agent skill, so a coding agent knows when to reach for sqlike on its own.

## Learn more

Try it at **[sqlike.com](https://sqlike.com)**. The equivalence checker is measured in public against
the standard academic benchmark, including a head-to-head with the state-of-the-art prover, at
**[sqlike.com/benchmark](https://sqlike.com/benchmark)**.

## Note

This repository is generated from the upstream monorepo, which is the source of truth. Please file
issues here. Code changes are made upstream and mirrored back.

## License

MIT OR Apache-2.0, at your option.
