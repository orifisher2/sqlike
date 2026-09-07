#!/usr/bin/env node
// Turn `sqlike check --json` (JSONL, one record per file) into GitHub workflow commands, so
// findings land inline on the pull-request diff.
//
// Workflow commands rather than SARIF: SARIF needs code scanning, which on a *private* repo
// requires paid GitHub Advanced Security — and the repos gating SQL in CI are mostly private.
// This costs nothing, needs no extra permission, and renders the same place.
//
// Reads JSONL on stdin, writes commands on stdout, and never decides whether the build fails —
// the CLI's exit code does that, and duplicating the gate here would let the two drift.

import { createInterface } from 'node:readline';

// GitHub renders only a handful of annotations per step. The full report is always in the job
// log; this decides which ones also get pinned to the diff, worst first.
const MAX_ANNOTATIONS = Number(process.env.SQLIKE_MAX_ANNOTATIONS ?? 10);

/** Workflow-command level for a finding. Only a real defect shouts. */
export function levelFor(severity, category) {
  if (category === 'validity') return 'error';
  if (category === 'correctness') return severity === 'high' ? 'error' : 'warning';
  return severity === 'low' ? 'notice' : 'warning';
}

const RANK = { error: 0, warning: 1, notice: 2 };

/** Escape per GitHub's workflow-command rules; an unescaped `::` or newline truncates a message. */
const esc = (s) => String(s).replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
const escProp = (s) => esc(s).replace(/:/g, '%3A').replace(/,/g, '%2C');

/** One `::level file=…::message` line. */
export function annotation(level, path, finding) {
  const span = finding.span?.start;
  const where = span ? `,line=${span.line},col=${span.column}` : '';
  const title = escProp(`sqlike: ${finding.rule}`);
  const body = esc(`${finding.title}\n${finding.what}`);
  return `::${level} file=${escProp(path)}${where},title=${title}::${body}`;
}

/** Every finding across every record, ranked so the cap keeps the ones that matter. */
export function collect(records) {
  return records
    .flatMap((r) => (r.findings ?? []).map((f) => ({
      level: levelFor(f.severity, f.category),
      path: r.path ?? '',
      finding: f,
    })))
    .sort((a, b) => RANK[a.level] - RANK[b.level]);
}

async function main() {
  const records = [];
  for await (const line of createInterface({ input: process.stdin })) {
    const t = line.trim();
    if (!t) continue;
    try {
      records.push(JSON.parse(t));
    } catch {
      // A non-JSON line is the CLI talking to a human (a warning on stderr that got merged in).
      // Pass it through rather than crash the annotator on it.
      process.stderr.write(`${t}\n`);
    }
  }

  const all = collect(records);
  for (const a of all.slice(0, MAX_ANNOTATIONS)) {
    console.log(annotation(a.level, a.path, a.finding));
  }
  const hidden = all.length - Math.min(all.length, MAX_ANNOTATIONS);
  if (hidden > 0) {
    // Say so explicitly: a silently truncated list reads as "sqlike found ten things".
    console.log(
      `::notice::${hidden} more finding${hidden === 1 ? '' : 's'} not shown as annotations — see the full report below`,
    );
  }

  // Every finding, uncapped, in the log. The annotation cap decides what is pinned to the diff;
  // it must never decide what the caller is allowed to know.
  if (all.length > 0) {
    console.log(`\nsqlike — ${all.length} finding${all.length === 1 ? '' : 's'}:`);
    for (const { level, path, finding } of all) {
      const at = finding.span?.start;
      const where = at ? `:${at.line}:${at.column}` : '';
      console.log(`  ${path}${where}  ${level}  ${finding.rule}  ${finding.title}`);
    }
  } else {
    console.log(`sqlike — no findings in ${records.length} file${records.length === 1 ? '' : 's'}.`);
  }
}

// Only run when invoked directly, so the tests can import the pure parts.
if (import.meta.url === `file://${process.argv[1]}`) await main();
