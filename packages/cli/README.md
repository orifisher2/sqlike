# @sqlike/cli

Command-line client for [sqlike](https://sqlike.com), a deterministic SQL static analyzer and
advisor (validity, anti-patterns, rewrites, index and schema advice) and query-equivalence checker
for Postgres, MySQL, MariaDB, SQLite, SQL Server, and DuckDB.

This package is a remote client. It tokenizes your SQL on your machine, which replaces identifiers
and literals with placeholders, and sends only the tokenized query to the sqlike API. There is no
analysis engine in it.

## Use it

```sh
# analyze a query
npx @sqlike/cli check query.sql

# or install it
npm i -g @sqlike/cli
brew install orifisher2/sqlike/sqlike

# from stdin, machine-readable
echo 'SELECT * FROM users WHERE id IN (SELECT uid FROM bans)' \
  | sqlike check - --json

# check that a rewrite is equivalent
sqlike diff before.sql after.sql --schema schema.sql
```

Pass `--dialect postgres|mysql|mariadb|sqlite|mssql|duckdb` (default postgres), `--schema <ddl-file>` for
column and type aware checks, and `--key <api-key>` for higher rate limits. If you have the query's
`EXPLAIN` output, pass it with `--explain <file>`: index findings are then confirmed or dismissed by
what the planner did. The schema and the plan are tokenized too.

Exit codes: `0` clean or equivalent, `1` a warning (with `--fail-on warn`) or differs, `2` a
blocking finding or undecided, `3` the check could not run, `4` usage error.

### Privacy

A query is tokenized before it leaves your machine. A query that does not parse is reported on
your machine and nothing is sent. A query that holds text the tokenizer cannot replace, such as a
password in `ALTER USER`, is not sent unless you pass `--allow-raw`.

## How it ships

The CLI is a native binary. Each platform's binary is published as its own optional-dependency
package (`@sqlike/cli-linux-x64`, `@sqlike/cli-darwin-arm64`, and so on), so npm installs only the
one matching your OS and CPU. No install scripts run, so it works with `--ignore-scripts`. Supported: linux x64 and arm64, macOS x64 and arm64, Windows x64.

## License

Apache-2.0. See the `LICENSE` file in the package.
