# npm packages

Public npm packages that let users run the sqlike MCP server via `npx` — no binary download, no
install scripts. These are thin remote clients; they contain no analysis engine.

- **`mcp/`** — `@sqlike/mcp`, the package users install. A Node launcher that locates and execs the
  native MCP server binary, passing stdio through unchanged.
- **`mcp-<platform>/`** — one package per target (`@sqlike/mcp-linux-x64`, `@sqlike/mcp-darwin-arm64`,
  …), each shipping a single prebuilt binary, guarded by `os`/`cpu` so npm installs only the match.
  The binary is a build artifact (git-ignored); populated at release time.

The launcher uses the optionalDependencies pattern (as esbuild/biome/swc do): npm resolves the one
platform package that matches, so there is no postinstall step and it works with `--ignore-scripts`.

## Release

Automated by `.github/workflows/release-mcp.yml`: **push a tag `mcp-v<version>`** (e.g. `mcp-v0.1.0`)
and it builds every target on native runners, runs `prepare-release.mjs`, and publishes the packages.
Needs an `NPM_TOKEN` secret (npm automation token for the `@sqlike` org). The `mcp-v*` tag namespace
is distinct from the server's `v*` deploy tags.

The manual steps the workflow performs, if you ever run it by hand:

Binaries are built from the `varq-mcp` crate (`cargo build -p varq-mcp --release`), one per target.
The MCP server is remote-only — it has no `local` feature and links no engine, so a plain release
build is the shippable artifact.

1. Build `sqlike-mcp` for every target and collect them into a directory named by target key:
   `sqlike-mcp-linux-x64`, `sqlike-mcp-linux-arm64`, `sqlike-mcp-darwin-x64`,
   `sqlike-mcp-darwin-arm64`, `sqlike-mcp-win32-x64.exe`. (macOS/Windows targets need their own
   runners or cross-compilation — a CI build matrix; a Linux dev box can only produce the Linux
   binaries.)
2. `node packages/scripts/prepare-release.mjs <version> <binaries-dir>` — copies each binary into its
   package and stamps every `package.json` to `<version>`.
3. Publish the platform packages first, then the meta package:
   `npm publish --access public` in each `mcp-<platform>/`, then in `mcp/`.

Publishing requires the `sqlike` npm org (packages are scoped `@sqlike/*`).
