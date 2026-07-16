# npm packages

Public npm packages that let users run the sqlike clients via `npx` — no binary download, no
install scripts. These are thin remote clients; they contain no analysis engine.

- **`mcp/`** — `@sqlike/mcp`, an MCP server. A Node launcher that locates and execs the native MCP
  server binary, passing stdio through unchanged.
- **`cli/`** — `@sqlike/cli`, the `sqlike` command-line tool. A Node launcher that locates and execs
  the native CLI binary, passing argv/stdio through unchanged.
- **`<product>-<platform>/`** — one package per target (`@sqlike/mcp-linux-x64`,
  `@sqlike/cli-darwin-arm64`, …), each shipping a single prebuilt binary, guarded by `os`/`cpu` so
  npm installs only the match. The binary is a build artifact (git-ignored); populated at release time.

The launchers use the optionalDependencies pattern (as esbuild/biome/swc do): npm resolves the one
platform package that matches, so there is no postinstall step and it works with `--ignore-scripts`.

## Release

Automated by `.github/workflows/release-mcp.yml` and `release-cli.yml`: **push a tag
`mcp-v<version>` or `cli-v<version>`** (e.g. `mcp-v0.1.0`) and it builds every target on native
runners, runs `prepare-release.mjs`, and publishes the packages. The CLI workflow also attaches the
binaries to a GitHub Release (the tarballs the Homebrew tap consumes). Needs an `NPM_TOKEN` secret
(npm automation token for the `@sqlike` org). The `mcp-v*` / `cli-v*` tag namespaces are distinct
from the server's `v*` deploy tags.

The manual steps the workflows perform, if you ever run one by hand (shown for `cli`; `mcp` is the
same with `sqlike-mcp` binaries and no GitHub Release):

The CLI is built remote-only (`cargo build -p varq-cli --release --no-default-features`) — the
`local` feature (the only path to the engine) is off, so a release build is the shippable artifact.

1. Build `sqlike` for every target and collect them into a directory named by target key:
   `sqlike-linux-x64`, `sqlike-linux-arm64`, `sqlike-darwin-x64`, `sqlike-darwin-arm64`,
   `sqlike-win32-x64.exe`. (macOS/Windows targets need their own runners or cross-compilation — a CI
   build matrix; a Linux dev box can only produce the Linux binaries.)
2. `node packages/scripts/prepare-release.mjs cli <version> <binaries-dir>` — copies each binary into
   its package and stamps every `package.json` to `<version>`.
3. Publish the platform packages first, then the meta package:
   `npm publish --access public` in each `cli-<platform>/`, then in `cli/`.

Publishing requires the `sqlike` npm org (packages are scoped `@sqlike/*`).
