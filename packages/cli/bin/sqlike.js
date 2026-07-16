#!/usr/bin/env node
'use strict';

// Launcher for the sqlike CLI. `sqlike` is a small native binary that forwards to
// the sqlike API (tokenizing locally first); each supported platform ships its
// binary as a dedicated optional-dependency package, so npm installs only the one
// whose os/cpu match and the user downloads a single binary. This script locates
// that binary and execs it, passing argv and stdio through unchanged.

const path = require('node:path');
const { spawnSync } = require('node:child_process');

// Keep in sync with optionalDependencies in package.json.
const PACKAGES = {
  'darwin-arm64': '@sqlike/cli-darwin-arm64',
  'darwin-x64': '@sqlike/cli-darwin-x64',
  'linux-arm64': '@sqlike/cli-linux-arm64',
  'linux-x64': '@sqlike/cli-linux-x64',
  'win32-x64': '@sqlike/cli-win32-x64',
};

function resolveBinary() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PACKAGES[key];
  if (!pkg) {
    throw new Error(
      `no prebuilt binary for ${key}; supported: ${Object.keys(PACKAGES).join(', ')}`,
    );
  }
  const exe = process.platform === 'win32' ? 'sqlike.exe' : 'sqlike';
  // Resolve via package.json (always accessible) then join the binary name.
  const pkgDir = path.dirname(require.resolve(`${pkg}/package.json`));
  return path.join(pkgDir, exe);
}

let bin;
try {
  bin = resolveBinary();
} catch (err) {
  process.stderr.write(
    `sqlike: ${err.message}. ` +
      `If your platform should be supported, reinstall without --no-optional.\n`,
  );
  process.exit(1);
}

// stdio: 'inherit' gives the child our real stdin/stdout/stderr so piping SQL in and
// reading the report out works unchanged; exit code is forwarded (the CLI uses it as
// a contract: 0 ok, 1 warn/differs, 2 block/undecided, 3 operational error).
const { status, signal, error } = spawnSync(bin, process.argv.slice(2), {
  stdio: 'inherit',
});
if (error) {
  process.stderr.write(`sqlike: failed to start ${bin}: ${error.message}\n`);
  process.exit(1);
}
process.exit(signal ? 1 : status ?? 0);
