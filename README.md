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

sqlike is the check in between. It flags unsafe patterns from a catalog of 170 rules, each one
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
# analyze a query — the hosted API is the default, so no flags are needed
sqlike check query.sql

# several files at once, with a schema
sqlike check migrations/*.sql --schema schema.sql

# reading from stdin, machine-readable output (one JSON record per file)
cat query.sql | sqlike check - --schema schema.sql --json

# check that a rewrite is equivalent
sqlike diff before.sql after.sql
```

Point it elsewhere with `--remote` or `SQLIKE_URL`, and authenticate with `--key` or
`SQLIKE_API_KEY`.

If you have the query's `EXPLAIN` output, pass it with `--explain plan.json` and the real access
paths will confirm or dismiss the index findings. The plan is tokenized before it leaves the machine,
same as the query and the schema.

Exit codes are a contract, so each one means exactly one thing:

| | `check` | `diff` |
|---|---|---|
| **0** | clean, or nothing at or above `--fail-on` | equivalent |
| **1** | a warning, when `--fail-on warn` asks for it | differs |
| **2** | a blocking defect: invalid SQL, or a high-severity correctness problem | undecided |
| **3** | operational — unreachable, rate-limited, or unparseable | same |
| **4** | usage error (a bad flag) | same |

`check` defaults to `--fail-on block`, so advisory findings report without failing a build. Ask for
`--fail-on warn` to be strict, or `never` to only report. **3 is never a verdict about your SQL** —
a pipeline can tell "this query is bad" from "the call did not go through".

## In CI, and on commit

Gate SQL where it is written. Both run the same checks as the CLI, and both tokenize locally first.

**GitHub Actions.** Findings land inline on the pull-request diff. The workflow authenticates as
your repository owner using GitHub's own OIDC token — no signup, no API key to store or leak.

```yaml
permissions:
  id-token: write        # authenticate as this repo's owner
  pull-requests: read    # check only the files this PR changes
  contents: read         # mode: diff, to read the earlier version of a file
steps:
  - uses: actions/checkout@v5
  - uses: orifisher2/sqlike@cli-v0.3.0
    with:
      dialect: postgres
      schema: db/schema.sql
```

Set `mode: diff` and it answers a different question on every pull request: *did this rewrite
change what the query returns?* Only modified files are compared, and a verdict of "cannot prove
either way" never fails a build.

**Pull requests from forks run anonymously.** GitHub does not issue OIDC tokens to workflows a fork
triggers, so a contributor's pull request cannot authenticate as your repository — it falls back to
an anonymous, per-IP rate limit. Nothing is broken and there is nothing to configure; your own
branches still authenticate normally. The action says so rather than suggesting a permission that
would not help.

Outside a pull request there is no set of changed files, so `changed-only` cannot apply and every
matching file is checked. The action warns when that happens, because on a large repository it is
enough requests to reach a rate limit.

**pre-commit.** Same checks before the commit lands:

```yaml
repos:
  - repo: https://github.com/orifisher2/sqlike
    rev: cli-v0.3.0
    hooks:
      - id: sqlike
        args: [--dialect, postgres]
```

The hook blocks a commit on a real defect, not on a bad connection: if sqlike cannot be reached it
says so and lets the commit through.

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
