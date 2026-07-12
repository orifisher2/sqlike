# sqlike — clients

Open-source clients for [sqlike](https://sqlike.com), a deterministic SQL static analyzer and
advisor (validity, anti-patterns, rewrites, schema/index advice, query equivalence) for Postgres,
MySQL, SQLite, and SQL Server.

These are **thin remote clients**. They tokenize your SQL **locally** — identifiers and literals
are masked before anything leaves your machine — and forward the tokenized query to the sqlike API
for analysis. They contain no analysis engine; that runs server-side and is closed.

## What's here

- **`crates/mcp`** — `sqlike-mcp`, an MCP (Model Context Protocol) server. Ships to npm as
  [`@sqlike/mcp`](packages/mcp); add it to any MCP client with:
  ```json
  { "mcpServers": { "sqlike": { "command": "npx", "args": ["-y", "@sqlike/mcp"] } } }
  ```
- **`crates/cli`** — `sqlike`, a command-line client (`sqlike check ... --remote https://api.sqlike.com`).
- **`crates/client`** — the shared, engine-free forwarder: tokenize → call API → detokenize.
- **`crates/core-parse`** — the SQL parser, stage model, tokenizer, and result types.
- **`packages/`** — the npm packaging for `@sqlike/mcp` (per-platform prebuilt binaries).

## Privacy

Tokenization happens here, on your machine, before any request. If a query can't be parsed it
can't be tokenized; the client then **refuses** to send it rather than transmit raw SQL, unless you
explicitly opt in (`allow_raw` / `--allow-raw`).

## Note

This repository is generated from the upstream monorepo (the source of truth). Please file issues
here; code changes are made upstream and mirrored.

## License

MIT OR Apache-2.0, at your option.
