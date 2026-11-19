# Using sqlike from an AI agent

[sqlike](https://sqlike.com) is a deterministic SQL analyzer. No language model takes part in its
analysis, so the same query always gets the same result. It reports validity errors and
anti-patterns, suggests rewrites and indexes, and checks whether two queries return the same
result.

This file applies to any agent. Claude Code users can install the skill in `skills/sqlike/`
(copy it into `.claude/skills/`), which gives the same instructions.

## Install

- MCP server (`@sqlike/mcp`): point your MCP client at `npx -y @sqlike/mcp`. It provides two
  tools, `analyze` and `diff`.
- CLI (`@sqlike/cli`): `npm i -g @sqlike/cli`, or `brew install orifisher2/sqlike/sqlike`. Then
  `sqlike check query.sql` and `sqlike diff a.sql b.sql`.

## When to use it

- After you write or edit SQL, run `analyze` before you present it, and fix what it reports.
- When you review SQL, run `analyze` and cite its findings.
- After you rewrite, refactor or optimize a query, run `diff` on the original and the rewrite.
  Reading two queries does not prove they return the same result. `diff` either proves it or says
  that it could not.

## SQL produced by an ORM or a query builder

sqlike reads SQL text. If the code builds its SQL through an ORM, a query builder or a template,
get the statement out first and analyze that.

How to print the statement:

- SQLAlchemy: `str(stmt)`, or `stmt.compile(dialect=postgresql.dialect())`.
- Django: `connection.queries` with `DEBUG=True` gives the SQL that was sent.
  `str(queryset.query)` is quicker but prints string parameters unquoted, so `WHERE name = Oscar`
  parses as a column.
- ActiveRecord: `relation.to_sql`.
- knex: `.toString()`, or `.toSQL()` for the SQL with its bindings.
- Drizzle: `.toSQL()` returns `{ sql, params }`.
- jOOQ: `query.getSQL()`, or `getSQL(ParamType.INLINED)` to inline the values.
- Diesel: `diesel::debug_query::<Pg, _>(&query).to_string()`, then remove the trailing
  `-- binds:` comment.
- Prisma: only through logging. Use `new PrismaClient({ log: ['query'] })` and read the `query`
  field of each event.

Keep bind placeholders as they are. `$1`, `?` and `:name` parse, and are listed in the result
under `parameters`. Python's `%s` and `%(name)s` do not parse: replace each with a `$N`
placeholder, never with a real value.

For SQL built from strings or conditions, analyze each complete statement the code can produce,
or at least the widest one. A `WHERE` fragment alone is not a query. For dbt, sqlc and other
templates, analyze the compiled output.

A change to ORM code that changes the SQL is a rewrite. Print the statement before and after and
run `diff` on the two, with the schema. An ORM reads columns by name, so pass
`columns: "by_name"` (CLI: `--columns by-name`). By default columns are compared by position, and
a moved column counts as a difference.

Take the schema from the migrations or the model definitions. Without a schema, column and type
checks are skipped.

## What to ask the user for

The query alone is often enough. Each further input changes the result in a specific way, so ask
only for the one the task needs, once, and give the user the command to run. All of it is
tokenized on the user's machine before it is sent (names and values are masked, row counts are
sent as numbers), so production output can be pasted.

| Input | What it changes | How the user gets it |
| --- | --- | --- |
| Dialect | Verdicts are per engine. Postgres is assumed if none is given. MySQL and MariaDB are separate dialects | Ask which database |
| Schema DDL (tables and indexes) | Unknown-column and type checks, type-aware rules, and all index advice (there is none without a schema). For `diff`, `NOT NULL` and unique keys let more pairs be proved | Postgres `pg_dump --schema-only -t <table>`, MySQL and MariaDB `SHOW CREATE TABLE`, SQLite and DuckDB `.schema <table>` |
| Table sizes (JSON map of table to row count) | Severity follows volume: a large table raises a performance finding, a small one lowers it to stylistic and removes index advice | Postgres `SELECT relname, reltuples::bigint FROM pg_class WHERE relname IN (...)`, MySQL `information_schema.tables.table_rows`, or `SELECT count(*)` |
| Execution plan | Missing-index findings are confirmed or dismissed by what the planner did. An actual plan also re-scores every performance finding by real row counts and flags estimates that are far from actual (stale statistics) | Postgres `EXPLAIN (ANALYZE, FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`, SQLite `.mode json` then `EXPLAIN QUERY PLAN`, SQL Server `SET STATISTICS XML ON` (actual) or `SET SHOWPLAN_XML ON` (estimated), DuckDB `EXPLAIN (FORMAT JSON)` |

What to ask for depends on the task:

- Writing a new query: ask for the schema before you write, because column names and types
  cannot be checked against a guess. Ask for table sizes if the tables are large or the user
  mentions performance. There is no plan yet.
- Reviewing a query: run with what you have. Ask for the schema only if a finding says a check
  was skipped, or the query names columns you cannot confirm.
- A query that is slow in production: ask for the actual plan first, then the schema with its
  indexes and the table sizes. Without the plan, findings are ranked by the shape of the query
  and the cause is not known. `EXPLAIN ANALYZE` executes the query. For a statement that writes,
  have the user wrap it in a transaction and roll back, and ask before running it on production.
- Refactoring or optimizing with the same results: ask for the schema with its constraints, for
  `diff`. Add the plan and the sizes if speed is the reason.
- Fixing an error: usually nothing more. Ask for the schema if the error is an unknown column or
  a type.

Do not hold up a simple task for extra data. Run with the query, report what you found, and say
which input would improve which finding. Table sizes are passed as `stats` and the plan as
`explain`, on the MCP `analyze` tool and on the CLI
(`sqlike check --stats sizes.json --explain plan.json`).

## Reading the results

`analyze` returns findings. Each has a severity (high, medium or low) and a category (validity,
correctness, performance, maintainability or portability). Fix every validity finding and every
high-severity finding, and report the rest with the query. Some findings include a rewrite that
can be applied as it is. Use it when there is one.

`diff` returns an `overall` verdict:

| Verdict | Meaning | What to do |
| --- | --- | --- |
| `Equivalent` | Proved to return the same result | The rewrite can replace the original |
| `EquivalentWithNotes` | The same data, with a difference in column names or in which query fixes the row order | Tell the user what differs |
| `Differs` | Proved to return a different result, or one of the two queries is invalid | Do not replace the original |
| `Undecided` | No proof either way | Say the rewrite is unverified. Do not call the queries equivalent |
| `NotComparable` | Both queries are invalid, each with a reason | Fix the queries first |

## Queries that cannot be masked

sqlike tokenizes a query on the user's machine before sending it: identifiers and literals are
replaced with placeholders. A query that does not parse is answered locally with a parse-error
finding, and nothing is sent.

Some queries hold text the tokenizer cannot replace, such as a password in `ALTER USER` or a file
path in `COPY`. For these, `analyze` refuses and returns a message that starts with `BLOCKED`.
Ask the user whether the raw query may be sent. If they agree, call again with `allow_raw=true`
(CLI: `--allow-raw`). Do not set `allow_raw` without asking: sending raw SQL off the machine is
the user's decision.
