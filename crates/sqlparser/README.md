# varq-sqlparser

A vendored copy of [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs), the parser
`core-parse` is built on. See `NOTICE` for the licence and attribution.

| | |
|---|---|
| upstream crate | `sqlparser` **0.62.0** |
| upstream commit | `3dd0e30d8bb1d2a6775f62d2b84839b60133effb` |
| vendored | 2026-09-12, phase PGV (`docs/phase-pgv-vendor-parser.md`) |

## Why it lives here

We reject SQL that real engines accept — 2,444 statements across the Postgres, MySQL and MariaDB
regression suites at the time of vendoring (`docs/plan-parser-and-verdicts.md`). Closing that gap
needs grammar changes. Upstream would take each one through review and a release every two to three
months, and three ways of working around the parser from outside were measured and found wanting.
Owning the copy means a construct is supported the day it is written.

## Rules for this directory

- **A file we change says so at the top.** The licence requires it (Apache-2.0 §4(b)), and it is
  how a reader tells our grammar from theirs. The header is one line:

  ```rust
  // MODIFIED from upstream sqlparser-rs 0.62.0 — see crates/sqlparser/README.md
  ```

- **Every change is listed below**, with the phase that made it. Small and localised beats clever:
  the fewer files we touch, the cheaper the next upstream pull.
- **Our lint gate does not run here.** `[lints.clippy] all = "allow"` in `Cargo.toml`. This is
  upstream code held to upstream's standards; our changes are reviewed in the PR that makes them.

## Modified files

None yet. PGV is a pure move; the first change belongs to PG1.

## Pulling a newer upstream

There is no automation for this, deliberately — a pull is a decision, made when upstream has
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
