# @sqlike/cli

Command-line client for [sqlike](https://sqlike.com), a deterministic SQL static analyzer and
advisor (validity, anti-patterns, rewrites, index and schema advice) and query-equivalence checker
for Postgres, MySQL, MariaDB, SQLite, and SQL Server.

This package is a **thin remote client**. It tokenizes your SQL locally, so identifiers and literals
are masked before anything leaves your machine, and forwards only the tokenized query to the sqlike
API. There is no analysis engine in it.

## Use it

```sh
# analyze a query
npx @sqlike/cli check query.sql --remote https://api.sqlike.com

# or install it
npm i -g @sqlike/cli
brew install orifisher2/sqlike/sqlike

# from stdin, machine-readable
echo 'SELECT * FROM users WHERE id IN (SELECT uid FROM bans)' \
  | sqlike check - --remote https://api.sqlike.com --json

# check a rewrite is equivalent (equivalence always runs server-side)
sqlike diff before.sql after.sql --schema schema.sql
```

Pass `--dialect postgres|mysql|mariadb|sqlite|mssql` (default postgres), `--schema <ddl-file>` for
column and type aware checks, and `--key <api-key>` for higher rate limits. If you have the query's
`EXPLAIN` output, `--explain <file>` lets the real access paths confirm or dismiss the index
findings. The schema and the plan are tokenized too.

Exit codes are a contract: `0` clean or equivalent, `1` advisory or differs, `2` blocking issue or
undecided, `3` operational error.

### Privacy

A query is tokenized before it leaves your machine. One that cannot be parsed cannot be tokenized,
so `check` and `diff` refuse rather than send raw SQL. Pass `--allow-raw` (with `--remote`) to
override that for a parse diagnostic.

## How it ships

The CLI is a native binary. Each platform's binary is published as its own optional-dependency
package (`@sqlike/cli-linux-x64`, `@sqlike/cli-darwin-arm64`, and so on), so npm installs only the
one matching your OS and CPU. No install scripts run, which means this works with `--ignore-scripts`
and in locked-down environments. Supported: linux x64 and arm64, macOS x64 and arm64, Windows x64.

## License

MIT OR Apache-2.0
