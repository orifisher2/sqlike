# sqlike: MCP server and CLI

[![@sqlike/mcp](https://img.shields.io/npm/v/%40sqlike%2Fmcp?label=%40sqlike%2Fmcp&color=17a673)](https://www.npmjs.com/package/@sqlike/mcp)
[![@sqlike/cli](https://img.shields.io/npm/v/%40sqlike%2Fcli?label=%40sqlike%2Fcli&color=17a673)](https://www.npmjs.com/package/@sqlike/cli)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue)](#license)

[sqlike](https://sqlike.com) reads a query and tells you what is wrong with it: validity errors,
anti-patterns, rewrites it can apply for you, and index advice. It also compares two queries and
reports whether the second one still returns the same results. No language model takes part in the
analysis, so the same query always gets the same answer.

This repository holds the clients: an [MCP](https://modelcontextprotocol.io) server, a CLI, and the
library they share. They tokenize your SQL on your machine, which replaces identifiers and literals with placeholders,
and send only the tokenized query. The analysis engine is not in this repo. It runs server-side and
is closed.

It supports Postgres, MySQL, MariaDB, SQLite, SQL Server, and DuckDB. Each rule has a verdict
measured on each engine, so the severity you get is the behaviour of your database. DuckDB is
columnar and has no general-purpose secondary index, so the ten index advisors are replaced there
by four columnar ones.

## Why

A query can read well and still be wrong. A `LEFT JOIN` that became an `INNER JOIN` drops rows. An
`UPDATE` that lost its `WHERE` changes every row. A join on the wrong key returns rows that look
plausible. These are easy to miss in review, whether a person or an AI model wrote the query.

sqlike checks the query before it runs. It reports unsafe patterns from a catalog of 172 rules,
each verified on a real database before it ships. It also checks whether a rewrite returns the same
results as the original. It answers `Equivalent` only when it has a proof, and `Undecided` when it
has none.

The analysis is deterministic, so two runs on the same input give the same result, and there is no
per-token cost. A pair the equivalence check decides takes a few milliseconds. The measurements are
at [sqlike.com/benchmark](https://sqlike.com/benchmark).

## Install the MCP server

Add it to any MCP client (Claude Code, Claude Desktop, Cursor, and so on):

```json
{
  "mcpServers": {
    "sqlike": { "command": "npx", "args": ["-y", "@sqlike/mcp"] }
  }
}
```

No key or account is needed. If you have an API key, set `SQLIKE_API_KEY` for higher rate limits.

## Tools

### `analyze`

Static analysis of one query: validity, anti-patterns, suggested rewrites, and schema and index
advice. Returns the JSON analysis envelope.

| Argument    | Type    | Description                                                              |
| ----------- | ------- | ------------------------------------------------------------------------ |
| `sql`       | string  | The query to analyze. **Required.**                                      |
| `schema`    | string  | Optional DDL (`CREATE TABLE` / `CREATE INDEX`) for column and type aware checks. |
| `stats`     | string  | Optional JSON map of table to row count, e.g. `{"orders": 2000000}`. Severity and index advice follow volume. |
| `explain`   | string  | Optional query plan (Postgres `EXPLAIN (ANALYZE, FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`, SQLite `EXPLAIN QUERY PLAN` as `.mode json` rows, SQL Server `SHOWPLAN_XML`, DuckDB `EXPLAIN (FORMAT JSON)`). Confirms or dismisses index findings by what the planner did. |
| `dialect`   | string  | `postgres` (default), `mysql`, `mariadb`, `sqlite`, `mssql`, or `duckdb`. |
| `allow_raw` | boolean | Send the query as typed when it holds text the tokenizer cannot replace, such as a password in `ALTER USER`. Default `false`. Ask the user first. |

### `diff`

Checks whether two queries return the same results. Returns a verdict (`Equivalent`,
`EquivalentWithNotes`, `Differs`, `Undecided`, or `NotComparable`), a confidence level, and a report
per property (columns, rows, cardinality, order), which shows what changed.

| Argument  | Type   | Description                                                        |
| --------- | ------ | ----------------------------------------------------------------- |
| `sql_a`   | string | The original query. **Required.**                                  |
| `sql_b`   | string | The rewritten query to check against `sql_a`. **Required.**        |
| `schema`  | string | Optional DDL both queries resolve against (one shared schema).     |
| `dialect` | string | `postgres` (default), `mysql`, `mariadb`, `sqlite`, `mssql`, `duckdb`. |
| `columns` | string | `by_position` (default) compares columns in order. `by_name` is for code that reads columns by name: a moved column is then a note. |

## CLI

The same checks from a terminal or a CI job. Run it without installing, or install it:

```sh
npx -y @sqlike/cli --help
npm i -g @sqlike/cli
brew install orifisher2/sqlike/sqlike
```

```sh
# analyze a query (the hosted API is the default)
sqlike check query.sql

# several files at once, with a schema
sqlike check migrations/*.sql --schema schema.sql

# read from stdin and print JSON, one record per file
cat query.sql | sqlike check - --schema schema.sql --json

# check that a rewrite is equivalent
sqlike diff before.sql after.sql
```

Use `--remote` or `SQLIKE_URL` for a different server, and `--key` or `SQLIKE_API_KEY` for an API
key.

If you have the query's `EXPLAIN` output, pass it with `--explain plan.json`. Index findings are
then confirmed or dismissed by what the planner did. The plan is tokenized before it is sent, like
the query and the schema.

Exit codes:

| | `check` | `diff` |
|---|---|---|
| 0 | clean, or nothing at or above `--fail-on` | equivalent |
| 1 | a warning, when `--fail-on warn` asks for it | differs |
| 2 | a blocking finding: invalid SQL, or a high-severity correctness problem | undecided |
| 3 | the check could not run: unreachable, rate-limited, or unparseable | same |
| 4 | usage error (a bad flag) | same |

`check` defaults to `--fail-on block`, so advisory findings are reported without failing a build.
Use `--fail-on warn` to fail on warnings, or `never` to only report. Exit code 3 says nothing about
the SQL, so a pipeline can tell a bad query from a call that did not go through.

`diff` exits 0 for equivalent, and also for notes such as a renamed column. A result's columns are
ordered, as SQL defines them, so a rewrite that moves a column differs (1). If your code reads
columns by name, `--columns by-name` compares each name's values instead, and a moved column is a
note. `--fail-on names,position,order` turns those notes into failures.

## In CI, and on commit

The GitHub Action and the pre-commit hook run the same checks as the CLI, and tokenize on your
machine first.

### GitHub Actions

Findings appear inline on the pull-request diff. The workflow authenticates as your repository
owner with GitHub's OIDC token, so there is no signup and no API key to store.

```yaml
permissions:
  id-token: write        # authenticate as this repo's owner
  pull-requests: read    # check only the files this PR changes
  contents: read         # mode: diff, to read the earlier version of a file
steps:
  - uses: actions/checkout@v5
  - uses: orifisher2/sqlike@cli-v0.4.5
    with:
      dialect: postgres
      schema: db/schema.sql
```

With `mode: diff`, each modified file is compared with its earlier version, and the action reports
whether the rewrite changed what the query returns. An `Undecided` verdict does not fail the build.
A moved column is a change, since columns are ordered. Set `columns: by-name` if your code reads
them by name.

Pull requests from forks run anonymously. GitHub does not issue OIDC tokens to workflows that a
fork triggers, so such a pull request is counted under the anonymous rate limit for its network.
There is nothing to configure, and your own branches authenticate as usual.

Outside a pull request there is no set of changed files, so `changed-only` does not apply and
every matching file is checked. The action warns when that happens, because on a large repository
it can reach a rate limit.

### pre-commit

The same checks before the commit is made:

```yaml
repos:
  - repo: https://github.com/orifisher2/sqlike
    rev: cli-v0.4.5
    hooks:
      - id: sqlike
        args: [--dialect, postgres]
```

The hook blocks a commit on a blocking finding. If sqlike cannot be reached, it says so and lets
the commit through.

## Privacy

Tokenization runs on your machine before any request is made. The server does not receive your
table names, column names or values.

A query that does not parse is reported on your machine and nothing is sent. A query that holds
text the tokenizer cannot replace, such as a password in `ALTER USER` or a path in `COPY`, is not
sent unless you opt in for that request (`allow_raw` on the MCP tool, `--allow-raw` on the CLI).

See [THREAT-MODEL.md](THREAT-MODEL.md) for what tokenization covers and what it does not.

## What is in this repo

- `crates/mcp`: `sqlike-mcp`, the MCP server. Published to npm as [`@sqlike/mcp`](packages/mcp).
- `crates/cli`: `sqlike`, the command-line client. Published to npm as [`@sqlike/cli`](packages/cli).
- `crates/client`: the code both share: tokenize, call the API, detokenize. It has no engine in it.
- `crates/core-parse`: the SQL parser, stage model, tokenizer, and result types.
- `packages/`: the npm packaging, with a prebuilt binary per platform.
- `skills/`: the skill file for coding agents. [AGENTS.md](AGENTS.md) has the same instructions
  for any agent.

## Learn more

Try it at [sqlike.com](https://sqlike.com). How the rules and the equivalence check are measured
is at [sqlike.com/benchmark](https://sqlike.com/benchmark).

## Note

This repository is generated from the upstream monorepo, which is the source of truth. Please file
issues here. Code changes are made upstream and mirrored back.

## License

Apache-2.0. See `LICENSE`.

`crates/sqlparser` is a vendored copy of [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs),
also Apache-2.0. Its `NOTICE` file records the upstream version and lists every file changed since.
