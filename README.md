# sqlike: MCP server & CLI

[![@sqlike/mcp](https://img.shields.io/npm/v/%40sqlike%2Fmcp?label=%40sqlike%2Fmcp&color=17a673)](https://www.npmjs.com/package/@sqlike/mcp)
[![@sqlike/cli](https://img.shields.io/npm/v/%40sqlike%2Fcli?label=%40sqlike%2Fcli&color=17a673)](https://www.npmjs.com/package/@sqlike/cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**The deterministic safety net for SQL — for the code you write and the code your AI writes.**

Your AI wrote a SQL query, or refactored one. sqlike checks it: the bugs and anti-patterns it
shipped, with one-click fixes — and whether a refactor is provably result-preserving. In about a
millisecond, and without your real data ever leaving your machine. Part of
[sqlike](https://sqlike.com).

These are **thin remote clients**: an [MCP](https://modelcontextprotocol.io) server, a CLI, and a
shared client library. They tokenize your SQL **locally** — identifiers and literals are masked
before anything leaves your machine — and forward only the tokenized query to the sqlike API. There
is no analysis engine here; that runs server-side and is closed.

## Why

59% of developers ship AI-generated code they don't fully understand, and AI SQL looks plausible
while being wrong more often than you'd like — a `LEFT JOIN` quietly becomes an `INNER` and drops
rows, a `WHERE` goes missing and updates everything, tables get joined the wrong way. Plausible is
not correct.

sqlike is the deterministic check that catches it — a **guardrail, not another prompt**. It **flags**
unsafe patterns from 160+ rules, each verified against a real database before it ships, and it
**proves** whether a rewrite preserves results. The equivalence check is sound rather than complete:
it certifies rewrites as safe, and when it can't prove one it says `Undecided` rather than guess, so
it never rubber-stamps a change that isn't safe. No model in the loop means no retry loops, no
per-token cost, and the same answer every time.

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

Static analysis of one SQL query: validity, anti-patterns, suggested rewrites, and schema/index
advice. Returns the JSON analysis envelope.

| Argument    | Type    | Description                                                              |
| ----------- | ------- | ------------------------------------------------------------------------ |
| `sql`       | string  | The SQL query to analyze. **Required.**                                  |
| `schema`    | string  | Optional DDL (`CREATE TABLE` / `CREATE INDEX`) for column- & type-aware checks. |
| `dialect`   | string  | `postgres` (default), `mysql`, `sqlite`, or `mssql`.                     |
| `allow_raw` | boolean | Only used when a query fails to parse (so can't be tokenized): send raw SQL for a parse diagnostic. Default `false`. |

### `diff`

Check whether two SQL queries are **equivalent** (result-preserving) — for verifying a rewrite or
refactor, a judgement an LLM cannot reliably self-grade. Returns a verdict (`Equivalent` /
`EquivalentWithNotes` / `Differs` / `Undecided`), a confidence level, and a **per-property** report
(columns, rows, cardinality, order), so you see *what* changed, not just a yes/no. `Undecided` never
means equivalent.

| Argument  | Type   | Description                                                        |
| --------- | ------ | ----------------------------------------------------------------- |
| `sql_a`   | string | The original query. **Required.**                                 |
| `sql_b`   | string | The rewritten query to check against `sql_a`. **Required.**       |
| `schema`  | string | Optional DDL both queries resolve against (one shared schema).    |
| `dialect` | string | `postgres` (default), `mysql`, `sqlite`, or `mssql`.              |

## CLI (for CI)

Run the same checks in a pipeline:

```sh
npx -y @sqlike/cli analyze query.sql
```

`crates/cli` builds `sqlike`, a command-line client (`--remote https://api.sqlike.com`).

## Private by design

Tokenization happens **here, on your machine, before any request** — sqlike never sees your real
table names, columns, or values. Nothing to leak, nothing to train on (an AI assistant needs the
real thing; sqlike doesn't). If a query can't be parsed it can't be tokenized, and the client
**refuses** to send it rather than transmit raw SQL, unless you explicitly opt in (`allow_raw` /
`--allow-raw`).

## What's here

- **`crates/mcp`**: `sqlike-mcp`, the MCP server. Ships to npm as [`@sqlike/mcp`](packages/mcp).
- **`crates/cli`**: `sqlike`, the command-line client.
- **`crates/client`**: the shared, engine-free forwarder: tokenize → call API → detokenize.
- **`crates/core-parse`**: the SQL parser, stage model, tokenizer, and result types.
- **`packages/`**: the npm packaging for `@sqlike/mcp` (per-platform prebuilt binaries).

## Learn more

Try it at **[sqlike.com](https://sqlike.com)**, or see how it's measured — including a head-to-head
against the state-of-the-art academic prover — at **[sqlike.com/benchmark](https://sqlike.com/benchmark)**.

## Note

This repository is generated from the upstream monorepo (the source of truth). Please file issues
here; code changes are made upstream and mirrored.

## License

MIT OR Apache-2.0, at your option.
