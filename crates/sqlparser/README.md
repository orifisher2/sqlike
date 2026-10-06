# varq-sqlparser

A vendored copy of [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs), the parser
`core-parse` is built on. See `NOTICE` for the licence and attribution.

| | |
|---|---|
| upstream crate | `sqlparser` **0.62.0** |
| upstream commit | `3dd0e30d8bb1d2a6775f62d2b84839b60133effb` |
| vendored | 2026-09-12, phase PGV (`docs/phase-pgv-vendor-parser.md`) |

## Why it lives here

We reject SQL that real engines accept: 2,517 statements across the Postgres, MySQL and MariaDB
regression suites at the time of vendoring (`docs/plan-parser-and-verdicts.md`). Closing that gap
needs grammar changes. Upstream would take each one through review and a release every two to three
months, and three ways of working around the parser from outside were measured and found wanting.
Owning the copy means a construct is supported the day it is written.

## Rules for this directory

- **A file we change says so at the top.** The licence requires it (Apache-2.0 §4(b)), and it is
  how a reader tells our grammar from theirs. The header is one line:

  ```rust
  // MODIFIED from upstream sqlparser-rs 0.62.0. See crates/sqlparser/README.md
  ```

- **Every change is listed below**, with the phase that made it. Small and localised beats clever:
  the fewer files we touch, the cheaper the next upstream pull.
- **Our lint gate does not run here.** `[lints.clippy] all = "allow"` in `Cargo.toml`. This is
  upstream code held to upstream's standards; our changes are reviewed in the PR that makes them.

## Modified files

| file | phase | change |
|---|---|---|
| `src/parser/mod.rs` | PG2 (layer 1) | Two structured `ParserError` variants, `Expected { expected, found }` and `At { message, location }`, produced by the three `expected*` helpers, the `parser_err!` macro and `From<TokenizerError>`. Each displays byte for byte as the string it replaces. Two upstream sites that passed a token where the macro wanted a location keep the plain string variant, since they never had a position. Two upstream tests that compared variants now compare the rendered text. |
| `src/parser/mod.rs` | PG2 (layer 3) | The "Expected an expression, found: FROM" site (a select list ending in a comma) reported the position of the token after `FROM`. It now reports `FROM` itself. |
| `src/parser/mod.rs` | PG1a | `parse_in` accepts a parenthesised scalar subquery as a list element (`IN ((SELECT 1), 2)`), accepted by all six engines. Takes the `InSubquery` branch only when the query is the whole parenthesised content, else rewinds and parses a list. Additive: diverges only where upstream rejected. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs`, `src/dialect/sqlite.rs` | PG1b | Parentheses around a lone table factor (`FROM (t)`, `FROM ((t))`, `FROM (t a)`). New narrow flag `supports_parenthesized_lone_table_factor` (bare `Table` only, no alias-after-parens, no paren-derived), set for MySQL/MariaDB; SQLite uses the existing coarse `supports_parens_around_table_factor`. Measured per engine so we never accept what an engine rejects. |
| `src/dialect/postgresql.rs`, `src/dialect/mysql.rs`, `src/dialect/mssql.rs`, `src/dialect/duckdb.rs` | PG1c | A nested join with deferred `ON` clauses (`a JOIN b JOIN c ON P1 ON P2`). Sets the existing upstream flag `supports_left_associative_joins_without_parens` to `false` for Postgres, MySQL/MariaDB, SQL Server and DuckDB, so the parser builds the right-nested tree `a JOIN (b JOIN c ON P1) ON P2` those engines evaluate. Measured per engine (result-equal to the explicit-paren form, inner and outer); SQLite rejects the construct, so it keeps the default and stays a gap. No parser code change, only the flag. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/postgresql.rs`, `src/dialect/mysql.rs`, `src/dialect/sqlite.rs`, `src/dialect/duckdb.rs` | PG1d | Three words every engine we ship accepts as an identifier. `TRIM` and a table-factor `UNNEST` are read as constructs only when a `(` follows, the guard `POSITION` already used. `TOP` gets a new flag `supports_select_top`, default `true` so no unmeasured dialect moves, turned off for Postgres, MySQL (covers MariaDB), SQLite and DuckDB: upstream parsed `TOP` everywhere, which rejected a column named `top` on five engines that accept it and accepted `SELECT TOP 10 x` on five that reject it. Measured on all six engines, both directions. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs`, `src/dialect/sqlite.rs` | RC2 | `x::type` is a cast only where the engine has the operator. New flag `supports_double_colon_cast`, default `true` so no unmeasured dialect moves, off for MySQL (covers MariaDB) and SQLite, whose engines refuse `SELECT x::int FROM t`. Gated at the infix arm, the only site that consumes the token (`parse_pg_cast` has no in-tree caller). `CAST(x AS t)` untouched. Priced off the reverse census: 2,058 of the 3,829 `pg-regress` statements our SQLite dialect accepted and SQLite refused were this token, and the fix closed about 11,100 across two dialects. |
| `src/ast/mod.rs`, `src/ast/spans.rs`, `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs` | PG1f | `CAST(x AS CHAR CHARACTER SET cs)` (`CHARSET` a synonym). `Expr::Cast` gains an `Option<ObjectName>` `charset` mirroring `Expr::Convert`; new flag `supports_cast_character_set` (default false) set for `MySqlDialect`. The charset is read only after `CHAR` and only on that flag, so `SIGNED`/`NCHAR` etc. still reject it. A trailing `COLLATE` is deliberately not parsed: MySQL rejects it, MariaDB accepts it, and they share this dialect, so parsing it would accept SQL MySQL refuses. `CHARSET` reprints as `CHARACTER SET`. Measured on all six engines. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs`, `src/dialect/mssql.rs`, `src/dialect/sqlite.rs` | RC3 | A reverse-census over-acceptance fix: `SELECT DISTINCT ON (...)` is Postgres/DuckDB only, but upstream parsed it everywhere. New flag `supports_distinct_on` (default true), set false for `MySqlDialect` (covers MariaDB), `SQLiteDialect`, `MsSqlDialect`; Postgres and DuckDB keep the default. `parse_all_or_distinct` consumes the `ON` and then errors where the flag is off, rather than leaving it to be misread as a function call. Measured on all six engines. See `docs/phase-rc3-distinct-on.md`. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs`, `src/dialect/sqlite.rs` | RC4 | A reverse-census over-acceptance fix: `TABLESAMPLE` is Postgres/SQL Server/DuckDB only (the existing `supports_table_sample_before_alias` flag only chose alias position, not whether the clause is allowed), but upstream parsed it everywhere. New flag `supports_table_sample` (default true), set false for `MySqlDialect` (covers MariaDB) and `SQLiteDialect`. `maybe_parse_table_sample` consumes the `TABLESAMPLE`/`SAMPLE` keyword (both reserved for aliases) and then errors where the flag is off. Measured on all six engines. See `docs/phase-rc4-tablesample.md`. |
| `src/parser/mod.rs` | RC5 | A reverse-census over-acceptance fix: `x IN UNNEST(...)` is a BigQuery operator, but upstream took the `UNNEST` branch of `parse_in` for every dialect. Guarded by `dialect_of!(self is BigQueryDialect \| GenericDialect)` checked before the keyword is consumed; none of the six we ship have it (SQLite parses `IN unnest(...)` as a table-valued function, a different construct, which we decline rather than misread). No new flag and no fall-through: the normal `IN (...)` path's `expect_token(LParen)` rejects it. Measured on all six engines. See `docs/phase-rc5-in-unnest.md`. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/postgresql.rs`, `src/dialect/mysql.rs`, `src/dialect/mssql.rs`, `src/dialect/sqlite.rs`, `src/dialect/duckdb.rs` | RC6 | A reverse-census over-acceptance fix: `FETCH FIRST n PERCENT ROWS` is an Oracle-family extension none of the six we ship accept (even the four with the plain FETCH clause), but upstream parsed it everywhere. New flag `supports_fetch_first_percent` (default true), set false for all six. In `parse_fetch` the `PERCENT` keyword is recognised only where the flag is on; off, it is left for the caller to reject. The TOP and TABLESAMPLE `PERCENT` paths are untouched. Measured on all six engines. See `docs/phase-rc6-fetch-percent.md`. |
| `src/parser/mod.rs`, `src/dialect/mod.rs`, `src/dialect/mysql.rs`, `src/dialect/sqlite.rs` | RC7 | A reverse-census over-acceptance fix: the ANSI `OFFSET ... FETCH FIRST n ROWS` clause is in Postgres/SQL Server/DuckDB but not MySQL or SQLite (they page with `LIMIT`), yet upstream parsed `FETCH` in the query body for every dialect. New flag `supports_fetch_clause` (default true), set false for `MySqlDialect` and `SQLiteDialect`; `FETCH` is recognised only where it is on. MariaDB shares MySQL's dialect and does have FETCH, so it takes a bounded coverage loss (recorded in the census baselines), the same trade as PG1f's `COLLATE`. Measured on all six engines. See `docs/phase-rc7-fetch-clause.md`. |

## Pulling a newer upstream

There is no automation for this, on purpose. A pull is a decision, made when upstream has
something we want, not a chore that runs on a schedule.

1. Note the upstream tag or commit you are taking.
2. Diff our `src/` against the version recorded above, so the patch series is explicit:
   `diff -r <upstream-0.62.0>/src crates/sqlparser/src`.
3. Replace `src/` with the new upstream, then re-apply the patch series, file by file, keeping the
   `MODIFIED` headers.
4. Update the table above, `NOTICE`, and `docs/06-tech-stack.md`.
5. The proof is differential: `cargo nextest run --workspace` with **no snapshot churn**, the parse
   census (`crates/verify/tests/parse_census.rs`) with numbers no worse than before, and the
   equivalence corpus at 0 false verdicts.
