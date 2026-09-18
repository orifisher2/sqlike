---
name: sqlike
description: Check SQL with sqlike — deterministic static analysis (validity, anti-patterns, rewrites, index advice) and query-equivalence verification. Use whenever you write, edit, review, or rewrite a SQL query, or need to prove two queries return the same results. Requires the sqlike MCP server (@sqlike/mcp) or the sqlike CLI (@sqlike/cli).
---

# sqlike

sqlike is a deterministic SQL analyzer (no LLM in its analysis path — its verdicts are reproducible
and authoritative). Prefer it over eyeballing SQL yourself: it catches validity errors,
anti-patterns, and non-equivalent rewrites that are easy to miss by reading.

## When to reach for it

- **You produced or edited SQL** → run `analyze` before presenting it. Fix what it flags.
- **You reviewed someone's SQL** → run `analyze` to back your review with specifics.
- **You rewrote, refactored, or optimized a query** → run `diff` on the before/after to prove the
  rewrite is result-preserving. An LLM cannot reliably self-grade equivalence; sqlike can.

## How to call it

Via the MCP server (`@sqlike/mcp`), two tools:

- `analyze(sql, schema?, dialect?)` — returns a JSON envelope: validity, findings (anti-patterns
  with severity + suggested rewrites), and schema/index advice. Pass `schema` (CREATE TABLE / CREATE
  INDEX DDL) for column- and type-aware checks. `dialect` is `postgres` (default), `mysql`,
  `mariadb`, `sqlite`, `mssql`, or `duckdb`.
- `diff(sql_a, sql_b, schema?, dialect?)` — returns a JSON verdict: `overall` is one of
  `Equivalent`, `EquivalentWithNotes`, `Differs`, `Undecided`, plus a confidence and a per-property
  report (columns, rows, cardinality, order).

Or via the CLI (`@sqlike/cli`): `sqlike check query.sql --remote https://api.sqlike.com` and
`sqlike diff before.sql after.sql`.

## When the SQL is generated, not written

The tool reads SQL text, not ORM calls or builder chains. If the code you wrote produces SQL
indirectly, get the statement out first and analyze that.

- **Dump the statement from the ORM.** SQLAlchemy: `str(stmt)` or
  `stmt.compile(dialect=postgresql.dialect())`. Django: `str(queryset.query)` for a quick look, but
  it prints string parameters unquoted (`WHERE name = Oscar` parses as a column, not a value); for
  the SQL actually sent use `connection.queries` with `DEBUG=True`. ActiveRecord: `relation.to_sql`.
  knex: `.toString()` (or `.toSQL()` for sql plus bindings). Drizzle: `.toSQL()` returns `{ sql,
  params }`. jOOQ: `query.getSQL()` (`getSQL(ParamType.INLINED)` to inline values). Diesel:
  `diesel::debug_query::<Pg, _>(&query).to_string()`, then drop the trailing `-- binds:` comment.
  Prisma exposes SQL only through logging: `new PrismaClient({ log: ['query'] })` and read the
  `query` field of each event.
- **Keep the bind placeholders.** `$1`, `?` and `:name` parse and are reported back under
  `parameters`. Python's `%s` and `%(name)s` do not parse and will hit the consent gate; replace each
  with a `$N` placeholder before analyzing, never with a real value.
- **Analyze the assembled statement, not the fragments.** For string-built or conditional SQL,
  produce each statement the code can actually emit (or at least the widest one) and analyze each.
  A `WHERE` fragment on its own is not a query.
- **Templated SQL is compiled first.** dbt, sqlc and similar: analyze the compiled output, not the
  Jinja or the annotated source.
- **An ORM refactor is a rewrite.** Changing a filter chain, replacing a join with a subquery, or
  swapping an ORM call for raw SQL changes the SQL. Dump the statement before and after and run
  `diff` on the pair, with the schema.
- **Pass the schema the ORM knows.** Migrations or model definitions give the DDL. Without it,
  column and type checks are skipped.

## What to ask the user for, and when

The query alone is often enough. Each extra input changes the answer in a specific way, so ask for
the one the task needs, once, with the exact command to run. Everything below is tokenized on the
machine before it is sent (names and values are masked; row counts travel as numbers), so it is
safe to paste production output.

| Input | What it changes | How the user gets it |
| --- | --- | --- |
| Dialect | Verdicts are per engine; Postgres is assumed if unsaid, and MySQL and MariaDB differ | Ask which database |
| Schema DDL (tables and indexes) | Unknown-column and type checks, type-aware rules, and all index advice (empty without a schema). For `diff`, `NOT NULL` and unique keys let more pairs be proved | Postgres `pg_dump --schema-only -t <table>`, MySQL/MariaDB `SHOW CREATE TABLE`, SQLite and DuckDB `.schema <table>` |
| Table sizes (JSON map of table to row count) | Severity scales with volume: a large table promotes a performance finding, a small one demotes it to stylistic and drops index advice | Postgres `SELECT relname, reltuples::bigint FROM pg_class WHERE relname IN (...)`, MySQL `information_schema.tables.table_rows`, or `SELECT count(*)` |
| Execution plan | Confirms or dismisses missing-index findings by what the planner actually did; an actual plan re-scores every performance finding by real row counts and flags estimate skew (stale statistics) | Postgres `EXPLAIN (ANALYZE, FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`, SQLite `.mode json` then `EXPLAIN QUERY PLAN`, SQL Server `SET STATISTICS XML ON` (actual) or `SET SHOWPLAN_XML ON` (estimated), DuckDB `EXPLAIN (FORMAT JSON)` |

By task:

- **"Write me a query" (new or complex).** Ask for the schema before writing; you cannot check
  column names or types against a guess. Ask for table sizes if the tables are large or the user
  mentions performance. There is nothing to run yet, so no plan.
- **"Review this query."** Run with what you have. Ask for the schema only if findings say a check
  was skipped or the query references columns you cannot confirm.
- **"It is slow in production."** Ask for the actual plan first (`ANALYZE`), then the schema with
  its indexes and the table sizes. Without the plan the findings are ranked by shape, not by cause,
  and the fix you propose is a guess. `EXPLAIN ANALYZE` executes the query: for anything that
  writes, have the user wrap it in a transaction and roll back, and ask before running on
  production.
- **"Refactor or optimize, same results."** The schema with its constraints, for `diff`. Add the
  plan and sizes if speed is the reason.
- **"Fix this error."** Usually nothing more. The schema if the error is an unknown column or type.

Do not block a simple task on extra data: run with the query, report what you found, and say which
input would sharpen which finding. Table sizes go in as `stats` and the plan as `explain`, on the
MCP `analyze` tool and on the CLI (`sqlike check --stats sizes.json --explain plan.json`).

## Reading the results

- **analyze findings** have a severity (high/medium/low) and category (validity, correctness,
  performance, maintainability, portability). Treat high/validity as must-fix; report the rest with
  the query. Some findings carry an auto-applicable rewrite — prefer it.
- **diff verdicts**: `Equivalent` = safe to swap. `EquivalentWithNotes` = same rows/data but a
  cosmetic difference (column names, ordering) — call it out. `Differs` = do NOT swap; the results
  change. **`Undecided` never means equivalent** — sqlike couldn't prove it either way, so treat the
  rewrite as unverified and say so; don't claim the queries are equivalent.

## Privacy / the consent gate (important)

sqlike **tokenizes the query locally** before anything leaves the machine — identifiers and literals
are masked, so the backend never sees real table names or data. A query that can't be parsed can't
be tokenized, so `analyze` **refuses** rather than send raw SQL, returning a `BLOCKED` message.

When you hit that refusal: **stop and ask the user** whether it's OK to send the raw (unparsed) query
off their machine for a parse diagnostic. Only if they agree, retry with `allow_raw=true` (CLI:
`--allow-raw`). Never set `allow_raw` on your own — it's the user's data-egress decision, not yours.
