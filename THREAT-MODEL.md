# sqlike: tokenization threat model

This document is for a security reviewer. It describes what the sqlike clients send, what the
backend can and cannot learn, and the risks that remain.

## Trust boundary

The client (the web app's WebAssembly module, the `sqlike` CLI, or the `@sqlike/mcp` server) runs
on your machine. The backend (`api.sqlike.com`) runs the closed analysis engine. The trust boundary
is the network hop between them. The design goal is that your SQL does not cross that boundary
with its real names and values.

## What the client does before anything is sent

1. It parses your query (and optional schema DDL, row-count stats, and `EXPLAIN` output) locally.
2. It tokenizes: every identifier (table name, column name, alias, function name where
   applicable) and every literal (strings, numbers, dates) is replaced with a placeholder. The
   mapping from real name to placeholder is built and kept on your machine.
3. Only the tokenized query is sent to the backend.
4. The response uses the same placeholders. The client maps them back, so you read findings
   with your real names.

The tokenizer is open source. Its code is in this repository (`crates/core-parse`), so you can
audit what is masked and what is not.

## What the backend receives

- The tokenized query: keywords, operators, the shape of joins, subqueries and CTEs, and
  placeholders in place of your names and values.
- If you provide them: tokenized schema structure, and row-count hints with the table names tokenized.
- Request metadata: a request ID, an API-key ID (if you use a key), a timestamp, the dialect, the
  analysis type, and success/failure. Standard transport metadata (e.g. source IP, TLS) applies as
  with any HTTPS request.
- What is kept: request counts per day for each caller. Without an API key, the caller is the
  first 16 bytes of `SHA-256(salt || network)`. The network is your IPv4 address or your IPv6 /64.
  The salt is random for each UTC day and deleted after two days. While it exists, someone who has
  it could check guessed addresses against the hash. After it is deleted, the counts can't be
  traced to an address, including by us. Rate-limit counters use the same hash.

## What the backend does not receive or store

- Your table names, column names, aliases and literal values. They are replaced before the request
  is sent, and the mapping stays on your machine.
- Request and response bodies are not stored. They are held in memory for the length of the
  analysis, and only the metadata above is logged.
- No language model takes part in the analysis, so your query is not sent to an AI system or used
  to train one.

## The exception: text the tokenizer cannot replace

If a query fails to parse, the client shows the parse error on your machine and sends nothing. If
a query parses but contains text the tokenizer can't replace (a password in `ALTER USER`, a path in
`COPY`, a comment it couldn't remove), it is not sent. The client checks its own payload and
refuses if it finds a string or comment it did not write. Sending such a query as typed is possible
only if the operator opts in for that request (`--allow-raw` on the CLI, `allow_raw: true` on the
MCP `analyze` tool). The MCP tool returns a `BLOCKED` message that tells the agent to ask the user
first. Without the opt-in, raw SQL is not sent. An organization can enforce this by policy, by
disallowing `--allow-raw` and `allow_raw`.

## Residual risks

Tokenization does not hide the following.

### Query shape

The backend sees the structure of your query: how many joins, how subqueries and CTEs nest, and
which operators you use. If the structure of a query is itself sensitive, note that it is not
masked.

### Numbers

Row-count statistics and the row estimates inside an `EXPLAIN`
plan are sent as plain numbers, attached to tokenized table names. The column types in your DDL,
and the number of tables and columns it declares, are visible too. None of this names anything,
but a table's size is not hidden.

### Placeholders within a request

Within one request, the same name maps to the same placeholder, which the analysis needs. The
mapping is not sent and bodies are not stored, so requests are not correlated with each other. A
hostile server could still observe that the same query shape was sent more than once.

### Transport metadata

Source IP, timing and request volume are visible to the backend and its infrastructure providers,
as with any hosted API.

### The deployed engine

The analysis engine is closed and runs server-side, so you cannot audit it as you can audit the
clients. The clients limit what this exposes: the engine does not receive your names or values, so
a compromised engine could read only the tokenized query.

## Supply-chain integrity

- The npm packages (`@sqlike/mcp`, `@sqlike/cli`, and their per-platform binaries) are published
  from GitHub Actions with npm provenance, a signed sigstore attestation that links each package to
  its source commit and build.
- Release binaries are checksummed (`SHA256SUMS` on each GitHub Release), and the Homebrew formula
  pins those hashes.
- The clients have few dependencies and contain no analysis engine.

## Out of scope

This document does not cover the server-side analysis engine, the internal controls of the
hosting providers, or the security of your own machine. See `SECURITY.md` for how to report
a vulnerability.
