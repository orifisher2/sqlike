# Security Policy

## Reporting a vulnerability

Please report security issues **privately** via GitHub's private vulnerability reporting:
the repository's **Security** tab → **Report a vulnerability**
([direct link](https://github.com/orifisher2/sqlike/security/advisories/new)).

Do not open a public issue for a suspected vulnerability. We aim to acknowledge a report within
3 business days.

## Scope

This repository contains the sqlike **clients only** — the CLI (`@sqlike/cli`) and the MCP server
(`@sqlike/mcp`). They are thin remote clients: they tokenize SQL locally and forward it to the
sqlike API. They contain **no analysis engine** — that runs server-side and is not in this repo.

In scope: the client code here (local tokenization of identifiers/literals, the forwarders, the npx
launchers) and the artifacts built from it (the published npm packages and the Homebrew formula).

## What the clients do with your data

Before anything leaves your machine, a query is **tokenized locally**: identifiers and literals are
replaced, so the backend never receives your real table names, column names, or values. A query
that cannot be parsed cannot be tokenized, so the clients **refuse to send it** unless you
explicitly opt in (`allow_raw` / `--allow-raw`).

## Supply chain

The release workflows publish each npm package from GitHub Actions with **npm provenance** — a
signed [sigstore](https://www.sigstore.dev/) attestation that links the package to the exact source
commit and build. Release binaries are checksummed (`SHA256SUMS` on each GitHub Release), and the
Homebrew formula pins those hashes.
