#!/usr/bin/env node
// Generate the Homebrew formula for the sqlike CLI from a release's checksums.
//
//   node packages/scripts/gen-homebrew-formula.mjs <version> <SHA256SUMS-file> > sqlike.rb
//
// <SHA256SUMS-file> is the `sha256sum` output attached to the cli-v<version> GitHub Release
// (lines: "<hash>  sqlike-<version>-<key>.tar.gz"). The formula pins the four unix tarballs by
// url + sha256; Homebrew picks the one matching the user's OS/CPU at install time.

import { readFileSync } from 'node:fs';

const [version, sumsFile] = process.argv.slice(2);
if (!version || !sumsFile) {
  console.error('usage: gen-homebrew-formula.mjs <version> <SHA256SUMS-file>');
  process.exit(1);
}

const sums = {};
for (const line of readFileSync(sumsFile, 'utf8').split('\n')) {
  const m = line.trim().match(/^([0-9a-f]{64})\s+\*?(.+)$/);
  if (m) sums[m[2]] = m[1];
}

const REPO = 'orifisher2/sqlike';
const url = (key) =>
  `https://github.com/${REPO}/releases/download/cli-v${version}/sqlike-${version}-${key}.tar.gz`;
const sha = (key) => {
  const h = sums[`sqlike-${version}-${key}.tar.gz`];
  if (!h) throw new Error(`missing sha256 for ${key} in ${sumsFile}`);
  return h;
};

process.stdout.write(`class Sqlike < Formula
  desc "Deterministic SQL static analysis and query-equivalence checking"
  homepage "https://sqlike.com"
  version "${version}"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "${url('darwin-arm64')}"
      sha256 "${sha('darwin-arm64')}"
    end
    on_intel do
      url "${url('darwin-x64')}"
      sha256 "${sha('darwin-x64')}"
    end
  end

  on_linux do
    on_arm do
      url "${url('linux-arm64')}"
      sha256 "${sha('linux-arm64')}"
    end
    on_intel do
      url "${url('linux-x64')}"
      sha256 "${sha('linux-x64')}"
    end
  end

  def install
    bin.install "sqlike"
  end

  test do
    assert_match "sqlike", shell_output("#{bin}/sqlike --help")
  end
end
`);
