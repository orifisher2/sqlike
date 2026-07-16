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
  `sqlite`, or `mssql`.
- `diff(sql_a, sql_b, schema?, dialect?)` — returns a JSON verdict: `overall` is one of
  `Equivalent`, `EquivalentWithNotes`, `Differs`, `Undecided`, plus a confidence and a per-property
  report (columns, rows, cardinality, order).

Or via the CLI (`@sqlike/cli`): `sqlike check query.sql --remote https://api.sqlike.com` and
`sqlike diff before.sql after.sql`.

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
