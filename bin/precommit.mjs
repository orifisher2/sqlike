#!/usr/bin/env node
// The `sqlike` pre-commit hook. `pre-commit` passes the hook's own `args:` followed by the staged
// filenames, and both are just arguments to `sqlike check`, so everything is forwarded as-is.
//
// Runs `sqlike` from the isolated environment `pre-commit` builds for this repo (its
// `@sqlike/cli` dependency puts the binary on PATH), so nothing is installed globally.

import { spawnSync } from 'node:child_process';

const argv = process.argv.slice(2);
// `files:` in the manifest already limits this to .sql, but a commit touching none of them still
// runs the hook with an empty list.
if (argv.length === 0) process.exit(0);

const result = spawnSync('sqlike', ['check', ...argv], { stdio: 'inherit' });

if (result.error?.code === 'ENOENT') {
  console.error(
    'sqlike: the CLI is not on PATH. `pre-commit` normally installs it from @sqlike/cli;\n' +
      'try `pre-commit clean && pre-commit install --install-hooks`.',
  );
  process.exit(1);
}

// 3 is operational — the server was unreachable, rate-limited, or a file could not be parsed.
// A commit must never be blocked because the network is down: someone on a plane still has to be
// able to commit. Say what happened and get out of the way. Exit 2 (a real defect in the SQL) and
// exit 1 (a warning, when asked for) still block, which is the point of the hook.
if (result.status === 3) {
  console.error('sqlike: could not check this commit (offline, rate-limited, or unparseable). Committing anyway.');
  process.exit(0);
}

process.exit(result.status ?? 1);
