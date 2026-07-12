#!/usr/bin/env node
// Populate the npm packages for a release: copy each built binary into its platform
// package and stamp every package.json to the target version.
//
//   node packages/scripts/prepare-release.mjs <version> <binaries-dir>
//
// <binaries-dir> holds the built binaries, one per platform, named by target triple key:
//   sqlike-mcp-linux-x64  sqlike-mcp-linux-arm64  sqlike-mcp-darwin-x64
//   sqlike-mcp-darwin-arm64  sqlike-mcp-win32-x64.exe
//
// After this, `npm publish` each platform package, then `@sqlike/mcp` last.

import { chmodSync, copyFileSync, existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PACKAGES_DIR = dirname(dirname(fileURLToPath(import.meta.url)));

// platform package dir -> [binary basename in <binaries-dir>, installed binary name]
const PLATFORMS = {
  'mcp-linux-x64': ['sqlike-mcp-linux-x64', 'sqlike-mcp'],
  'mcp-linux-arm64': ['sqlike-mcp-linux-arm64', 'sqlike-mcp'],
  'mcp-darwin-x64': ['sqlike-mcp-darwin-x64', 'sqlike-mcp'],
  'mcp-darwin-arm64': ['sqlike-mcp-darwin-arm64', 'sqlike-mcp'],
  'mcp-win32-x64': ['sqlike-mcp-win32-x64.exe', 'sqlike-mcp.exe'],
};

const [version, binariesDir] = process.argv.slice(2);
if (!version || !binariesDir) {
  console.error('usage: prepare-release.mjs <version> <binaries-dir>');
  process.exit(1);
}

function setVersion(pkgDir, deps) {
  const file = join(PACKAGES_DIR, pkgDir, 'package.json');
  const pkg = JSON.parse(readFileSync(file, 'utf8'));
  pkg.version = version;
  if (deps && pkg.optionalDependencies) {
    for (const name of Object.keys(pkg.optionalDependencies)) {
      pkg.optionalDependencies[name] = version;
    }
  }
  writeFileSync(file, `${JSON.stringify(pkg, null, 2)}\n`);
}

const missing = [];
for (const [pkgDir, [srcName, destName]] of Object.entries(PLATFORMS)) {
  const src = join(binariesDir, srcName);
  if (!existsSync(src)) {
    missing.push(srcName);
    continue;
  }
  const dest = join(PACKAGES_DIR, pkgDir, destName);
  copyFileSync(src, dest);
  if (!destName.endsWith('.exe')) chmodSync(dest, 0o755);
  setVersion(pkgDir, false);
  console.log(`${pkgDir}: ${srcName} -> ${destName} @ ${version}`);
}

setVersion('mcp', true);
console.log(`mcp: @ ${version} (optionalDependencies pinned)`);

if (missing.length) {
  // Loud, not fatal: a partial release (e.g. before macOS builds land) is a real state,
  // but the caller must know which platforms are unpublished.
  console.warn(`\nWARNING: missing binaries, these platforms were NOT populated:\n  ${missing.join('\n  ')}`);
}
