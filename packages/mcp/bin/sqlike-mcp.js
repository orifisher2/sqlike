#!/usr/bin/env node
'use strict';

// Launcher for the sqlike MCP server. The server is a small native binary that
// speaks MCP over stdio and forwards to the sqlike API; each supported platform
// ships its binary as a dedicated optional-dependency package, so npm installs
// only the one whose os/cpu match and the user downloads a single binary.
// This script locates that binary and execs it, passing stdio through unchanged.

const path = require('node:path');
const { spawnSync } = require('node:child_process');

// Keep in sync with optionalDependencies in package.json.
const PACKAGES = {
  'darwin-arm64': '@sqlike/mcp-darwin-arm64',
  'darwin-x64': '@sqlike/mcp-darwin-x64',
  'linux-arm64': '@sqlike/mcp-linux-arm64',
  'linux-x64': '@sqlike/mcp-linux-x64',
  'win32-x64': '@sqlike/mcp-win32-x64',
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PACKAGES[key];
  if (!pkg) {
    throw new Error(
      `no prebuilt binary for ${key}; supported: ${Object.keys(PACKAGES).join(', ')}`,
    );
  }
  const exe = process.platform === 'win32' ? 'sqlike-mcp.exe' : 'sqlike-mcp';
  // Resolve via package.json (always accessible) then join the binary name.
  const pkgDir = path.dirname(require.resolve(`${pkg}/package.json`));
  return path.join(pkgDir, exe);
}

let bin;
try {
  bin = resolveBinary();
} catch (err) {
  process.stderr.write(
    `sqlike-mcp: ${err.message}. ` +
      `If your platform should be supported, reinstall without --no-optional.\n`,
  );
  process.exit(1);
}

// stdio: 'inherit' gives the child our real stdin/stdout so MCP's JSON-RPC stream
// passes through byte-for-byte; the child reads SQLIKE_URL / SQLIKE_API_KEY from env.
const { status, signal, error } = spawnSync(bin, process.argv.slice(2), {
  stdio: 'inherit',
});
if (error) {
  process.stderr.write(`sqlike-mcp: failed to start ${bin}: ${error.message}\n`);
  process.exit(1);
}
process.exit(signal ? 1 : status ?? 0);
