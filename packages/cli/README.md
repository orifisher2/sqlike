# @sqlike/cli

Command-line client for [sqlike](https://sqlike.com) — a deterministic SQL static analyzer and
advisor (validity, anti-patterns, rewrites, schema advice) and query-equivalence checker for
Postgres, MySQL, SQLite, and SQL Server.

This package is a **thin remote client**: it tokenizes your SQL locally (identifiers and literals
are replaced before anything leaves your machine) and forwards the tokenized query to the sqlike
API. It contains no analysis engine.

## Use it

```sh
# analyze a query
npx @sqlike/cli check query.sql --remote https://api.sqlike.com

# or install it
npm i -g @sqlike/cli
sqlike check query.sql --remote https://api.sqlike.com

# from stdin, machine-readable
echo 'SELECT * FROM users WHERE id IN (SELECT uid FROM bans)' \
  | sqlike check - --remote https://api.sqlike.com --json

# check a rewrite is equivalent (equivalence always runs server-side)
sqlike diff before.sql after.sql --schema schema.sql
```

Pass `--dialect postgres|mysql|sqlite|mssql` (default postgres), `--schema <ddl-file>` for
column- and type-aware checks, and `--key <api-key>` for higher rate limits.

Exit codes are a contract: `0` clean / equivalent, `1` advisory / differs, `2` blocking issue /
undecided, `3` operational error.

### Privacy

A query is tokenized before it leaves your machine. A query that can't be parsed can't be
tokenized, so `check`/`diff` refuse rather than send raw SQL; pass `--allow-raw` (with `--remote`)
to override for a parse diagnostic.

## How it ships

The CLI is a native binary. Each platform's binary is published as its own optional-dependency
package (`@sqlike/cli-linux-x64`, `@sqlike/cli-darwin-arm64`, …); npm installs only the one matching
your OS and CPU. No install scripts run — this works with `--ignore-scripts` and in locked-down
environments. Supported: linux x64/arm64, macOS x64/arm64, Windows x64. Also available via Homebrew
(`brew install orifisher2/sqlike/sqlike`).

## License

MIT OR Apache-2.0
