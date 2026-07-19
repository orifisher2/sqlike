# sqlike — tokenization threat model

This document describes, for a security reviewer, exactly what the sqlike clients send, what the
backend can and cannot learn, and the residual risks. It is deliberately honest about limits: a
threat model that only lists strengths is not useful.

## Trust boundary

The **client** (the web app's WebAssembly module, the `sqlike` CLI, or the `@sqlike/mcp` server)
runs on your machine. The **backend** (`api.sqlike.com`) runs the closed analysis engine
server-side. The trust boundary is the network hop between them. The design goal is that **your real
SQL never crosses that boundary in the clear**.

## What the client does before anything is sent

1. It parses your query (and optional schema DDL, row-count stats, and `EXPLAIN` output) locally.
2. It **tokenizes**: every identifier (table name, column name, alias, function name where
   applicable) and every literal (strings, numbers, dates) is replaced with an opaque placeholder.
   The mapping from real name → placeholder is built and kept **on your machine**.
3. Only the tokenized structure is sent to the backend.
4. The response (which references the same placeholders) is **detokenized locally**, so you read
   findings against your real names.

The tokenizer is open source. The exact code that runs is in this repository
(`crates/core-parse`), so you can audit what is and is not masked.

## What the backend receives

- The tokenized query structure: keywords, operators, the shape of joins/subqueries/CTEs, and
  opaque placeholders in place of your names and values.
- If you provide them: tokenized schema structure, and row-count hints with the table names tokenized.
- Request metadata: a request ID, an API-key ID (if you use a key), a timestamp, the dialect, the
  analysis type, and success/failure. Standard transport metadata (e.g. source IP, TLS) applies as
  with any HTTPS request.

## What the backend does **not** receive or store

- Your real table names, column names, aliases, or literal values — these are replaced before
  transmission and the mapping never leaves your machine.
- Tokenized **request and response bodies are not stored.** They are processed in memory to produce
  the analysis and then discarded; only the metadata above is logged. (See the service's data-
  handling statement for retention specifics.)
- There is no language model anywhere in the analysis path, so your query is never sent to, or used
  to train, any AI system.

## The one egress exception: unparseable queries

Tokenization requires a parseable query. If a query fails to parse it **cannot** be tokenized. The
clients **fail closed**: they refuse to send it rather than transmit raw SQL. Sending the raw query
for a parse diagnostic is possible only if the operator explicitly opts in per request
(`--allow-raw` on the CLI, `allow_raw: true` on the MCP `analyze` tool). The MCP tool surfaces a
`BLOCKED` message instructing the agent to ask the user first. **If you never opt in, raw SQL is
never sent.** Enterprises can enforce this by policy (disallow `--allow-raw` / `allow_raw`).

## Residual risks (what tokenization does *not* hide)

Be aware of these; they are inherent to sending anything at all:

- **Query shape is visible.** The backend sees the structure of your query — how many joins, the
  nesting of subqueries/CTEs, which operators you use. For most teams this is harmless; if your
  *query structure itself* encodes sensitive business logic, note that it is not masked.
- **Placeholder correlation within a request.** The same real name maps to the same placeholder
  within a single request (that is what makes the analysis correct). The mapping is not sent, and
  bodies are not stored, so cross-request correlation is not performed; but a hostile server could
  in principle observe repeated *shapes*.
- **Transport metadata.** Source IP, timing, and request volume are visible to the backend and its
  infrastructure providers, as with any hosted API.
- **You are trusting the deployed engine.** The analysis engine is closed and runs server-side; you
  cannot audit it the way you can audit the (open-source) clients. The mitigation is structural: the
  clients guarantee the engine never receives your real names or values, so a compromised or
  malicious engine still cannot read your data — only the tokenized shape.

## Supply-chain integrity

- The npm packages (`@sqlike/mcp`, `@sqlike/cli`, and their per-platform binaries) are published
  from GitHub Actions with **npm provenance** — a signed sigstore attestation linking each package
  to the exact source commit and build.
- Release binaries are checksummed (`SHA256SUMS` on each GitHub Release); the Homebrew formula pins
  those hashes.
- The clients are dependency-light forwarders and contain no analysis engine.

## Out of scope

The closed server-side analysis engine, the hosting infrastructure's internal controls, and the
security of your own machine are out of scope for this document. See `SECURITY.md` for how to report
a vulnerability.
