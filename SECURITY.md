# Security Policy

## Reporting a vulnerability

Report security issues privately, through GitHub's
[private vulnerability reporting](https://github.com/orifisher2/sqlike/security/advisories/new)
(the repository's Security tab, then "Report a vulnerability"), or by email to
security@sqlike.com.

Do not open a public issue for a suspected vulnerability. We aim to acknowledge a report within
3 business days.

## Scope

This repository contains the sqlike clients: the CLI (`@sqlike/cli`), the MCP server
(`@sqlike/mcp`), the GitHub Action and the pre-commit hook. They tokenize SQL on your machine and
send it to the sqlike API. The analysis engine runs server-side and is not in this repo.

In scope: the client code here (local tokenization of identifiers/literals, the forwarders, the npx
launchers) and the artifacts built from it (the published npm packages and the Homebrew formula).

## What the clients do with your data

A query is tokenized on your machine before it is sent: identifiers and literals are replaced with
placeholders, so the backend does not receive your table names, column names or values. A query
that does not parse is reported on your machine and nothing is sent. A query that holds text the
tokenizer cannot replace is not sent unless you opt in for that request (`--allow-raw` on the CLI,
`allow_raw` on the MCP tool).

[THREAT-MODEL.md](THREAT-MODEL.md) has the full analysis: the trust boundary, what the backend can
and cannot infer, and the residual risks.

## Supply chain

The release workflows publish each npm package from GitHub Actions with npm provenance, a signed
[sigstore](https://www.sigstore.dev/) attestation that links the package to its source commit and
build. Release binaries are checksummed (`SHA256SUMS` on each GitHub Release), and the
Homebrew formula pins those hashes.
