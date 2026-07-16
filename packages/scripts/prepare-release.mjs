#!/usr/bin/env node
// Populate the npm packages for a release: copy each built binary into its platform
// package and stamp every package.json to the target version.
//
//   node packages/scripts/prepare-release.mjs <product> <version> <binaries-dir>
//
// <product> is `mcp` or `cli`. <binaries-dir> holds the built binaries, one per platform,
// named by target key, e.g. for cli:
//   sqlike-linux-x64  sqlike-linux-arm64  sqlike-darwin-x64  sqlike-darwin-arm64  sqlike-win32-x64.exe
//
// After this, `npm publish` each platform package, then the `@sqlike/<product>` meta package last.

import { chmodSync, copyFileSync, existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PACKAGES_DIR = dirname(dirname(fileURLToPath(import.meta.url)));

// Binary name per product; the meta package dir is the product itself.
const BINARIES = { mcp: 'sqlike-mcp', cli: 'sqlike' };
const KEYS = ['linux-x64', 'linux-arm64', 'darwin-x64', 'darwin-arm64', 'win32-x64'];

const [product, version, binariesDir] = process.argv.slice(2);
if (!BINARIES[product] || !version || !binariesDir) {
  console.error('usage: prepare-release.mjs <mcp|cli> <version> <binaries-dir>');
  process.exit(1);
}
const bin = BINARIES[product];

// platform package dir -> [binary basename in <binaries-dir>, installed binary name]
const platforms = Object.fromEntries(
  KEYS.map((key) => {
    const ext = key === 'win32-x64' ? '.exe' : '';
    return [`${product}-${key}`, [`${bin}-${key}${ext}`, `${bin}${ext}`]];
  }),
);

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
for (const [pkgDir, [srcName, destName]] of Object.entries(platforms)) {
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

setVersion(product, true);
console.log(`${product}: @ ${version} (optionalDependencies pinned)`);

if (missing.length) {
  // Loud, not fatal: a partial release (e.g. before macOS builds land) is a real state,
  // but the caller must know which platforms are unpublished.
  console.warn(`\nWARNING: missing binaries, these platforms were NOT populated:\n  ${missing.join('\n  ')}`);
}
