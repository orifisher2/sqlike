//! `sqlike check` — analyze one or more SQL files and report findings.
//!
//! Split out of `main.rs` when multi-path checking arrived: the command carries its own gate,
//! its own exit-code policy, and all of the human rendering, which is most of what a CLI is.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use owo_colors::{OwoColorize, Stream};

use varq_core_parse::enrich::{FindingSort, RenderedResult};
#[cfg(feature = "local")]
use varq_core_parse::plan::Plan;
use varq_core_parse::result::{Category, Outcome, Severity};
#[cfg(feature = "local")]
use varq_core_parse::schema::Stats;
use varq_core_parse::Dialect;
// The local analysis engine is the only `varq-core` reference, gated behind the `local` feature.
// A remote-only build (`--no-default-features`) doesn't link `core` at all — the distributable,
// publishable client, a pure forwarder. Every other type here comes from the public `core-parse`.
#[cfg(feature = "local")]
use varq_core::analyze_with_plan;

use crate::{read_input, resolve_key, resolve_remote};

/// Parse a comma-separated `--sort` spec (e.g. `severity,type,location`) into ordered keys; each
/// key after the first breaks ties of the previous.
fn parse_sort_keys(s: &str) -> Result<Vec<FindingSort>> {
    s.split(',')
        .map(|k| match k.trim() {
            "severity" => Ok(FindingSort::Severity),
            "type" => Ok(FindingSort::Category),
            "location" | "position" => Ok(FindingSort::Location),
            other => {
                anyhow::bail!("unknown sort key `{other}` (expected severity, type, or location)")
            }
        })
        .collect()
}

/// How bad a result has to be before `check` fails the process. Expressed in [`Outcome`] terms
/// because that model is already category-aware (`result.rs`): only Validity and Correctness·High
/// block, so `block` is "a real defect" rather than "some high-severity finding".
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FailOn {
    Never,
    Warn,
    Block,
}

pub fn parse_fail_on(s: &str) -> Result<FailOn, String> {
    match s {
        "never" => Ok(FailOn::Never),
        "warn" => Ok(FailOn::Warn),
        "block" => Ok(FailOn::Block),
        other => Err(format!(
            "unknown level `{other}` (expected never, warn, or block)"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_check(
    queries: Vec<PathBuf>,
    schema: Option<PathBuf>,
    stats: Option<PathBuf>,
    explain: Option<PathBuf>,
    dialect: Dialect,
    sort: String,
    json: bool,
    remote: Option<String>,
    key: Option<String>,
    fail_on: FailOn,
    allow_raw: bool,
) -> Result<ExitCode> {
    let remote = resolve_remote(remote);
    let key = resolve_key(key);
    let sort_keys = parse_sort_keys(&sort)?;
    // Schema, stats and plan describe the database, not the query, so they are read once and
    // shared across every file in the run.
    let schema_ddl = schema
        .as_deref()
        .map(|p| {
            std::fs::read_to_string(p).with_context(|| format!("reading schema {}", p.display()))
        })
        .transpose()?;
    // Keep the raw JSON for the remote path (the client tokenizes its table-name keys); parse it
    // for the local path. Parsing here also validates a bad file regardless of path.
    let stats_json = stats
        .as_deref()
        .map(|p| {
            std::fs::read_to_string(p).with_context(|| format!("reading stats {}", p.display()))
        })
        .transpose()?;
    #[cfg(feature = "local")]
    let stats = stats_json
        .as_deref()
        .map(|j| Stats::from_json(j).map_err(|e| anyhow::anyhow!(e)))
        .transpose()?;
    // Raw EXPLAIN JSON: parsed to a plan for the local path; the remote path hands it to the
    // client, which tokenizes it (identifiers only) before it leaves the machine.
    let explain_json = explain
        .as_deref()
        .map(|p| {
            std::fs::read_to_string(p).with_context(|| format!("reading explain {}", p.display()))
        })
        .transpose()?;
    #[cfg(feature = "local")]
    let plan = explain_json
        .as_deref()
        .map(|j| Plan::from_explain(j, dialect).map_err(|e| anyhow::anyhow!(e)))
        .transpose()?;

    // One path is the common case and prints exactly as it always has; more than one prefixes
    // each finding with its file, because "12:8: SELECT *" is useless across twenty migrations.
    let label_paths = queries.len() > 1;
    let mut worst = Outcome::Ok;

    for query in &queries {
        let sql = read_input(query)?;
        let mut result = match &remote {
            Some(url) => {
                let r = varq_client::analyze(
                    url,
                    key.as_deref(),
                    &sql,
                    schema_ddl.as_deref(),
                    stats_json.as_deref(),
                    explain_json.as_deref(),
                    dialect,
                    allow_raw,
                )?;
                if r.dialect != dialect {
                    eprintln!(
                        "warning: server analyzed as {}, not {dialect} — it predates dialect \
                         support; update the server",
                        r.dialect
                    );
                }
                r
            }
            #[cfg(feature = "local")]
            None => analyze_with_plan(
                &sql,
                schema_ddl.as_deref(),
                stats.as_ref(),
                plan.as_ref(),
                dialect,
            )
            .rendered(),
            // `resolve_remote` always yields a URL without the local engine, so this cannot happen.
            #[cfg(not(feature = "local"))]
            None => unreachable!("a remote-only build defaults --remote to the hosted API"),
        };
        result.sort(&sort_keys);
        worst = worst.max(result.outcome());

        if json {
            println!("{}", jsonl_line(&result, query)?);
        } else {
            print_human(&result, label_paths.then(|| query.display().to_string()));
        }
    }

    Ok(exit_code(worst, fail_on))
}

/// One JSONL record: the documented envelope, compact, plus the file it came from.
///
/// JSONL for every invocation rather than an envelope for one path and JSONL for many — the
/// output shape must not depend on how many files a caller happened to pass, or every wrapper
/// has to branch on it. Only the pretty-printing changes; the fields are as documented.
fn jsonl_line(result: &RenderedResult, path: &std::path::Path) -> Result<String> {
    let mut value: serde_json::Value =
        serde_json::from_str(&result.to_json()).context("re-reading the analysis envelope")?;
    let obj = value
        .as_object_mut()
        .context("the analysis envelope is not an object")?;
    obj.insert(
        "path".to_string(),
        serde_json::Value::String(path.display().to_string()),
    );
    Ok(value.to_string())
}

/// Exit code from the category-aware policy in `core`, gated by `--fail-on`: a broken/wrong query
/// blocks (2), advisories warn (1), clean passes (0). Anything below the threshold reports 0 —
/// which is what `result.rs` already documents ("the advisory categories… never fail CI") and what
/// the old unconditional Warn→1 mapping contradicted.
fn exit_code(outcome: Outcome, fail_on: FailOn) -> ExitCode {
    let fails = match fail_on {
        FailOn::Never => false,
        FailOn::Warn => matches!(outcome, Outcome::Warn | Outcome::Block),
        FailOn::Block => outcome == Outcome::Block,
    };
    if !fails {
        return ExitCode::SUCCESS;
    }
    match outcome {
        Outcome::Block => ExitCode::from(2),
        Outcome::Warn => ExitCode::from(1),
        Outcome::Ok => ExitCode::SUCCESS,
    }
}

fn category_name(c: Category) -> &'static str {
    match c {
        Category::Validity => "validity",
        Category::Correctness => "correctness",
        Category::Performance => "performance",
        Category::Maintainability => "maintainability",
        Category::Portability => "portability",
    }
}

fn print_human(r: &RenderedResult, path: Option<String>) {
    print_parameters(r);

    // `path:` on every line rather than a header per file: the output is grep-able and each
    // line stands alone, which is what a caller scrolling a CI log actually needs.
    let prefix = path.map(|p| format!("{p}:")).unwrap_or_default();

    if r.findings.is_empty() && r.advice.is_empty() && r.hotspots.is_empty() {
        if !prefix.is_empty() {
            println!(
                "{} {prefix} no issues found",
                "✓".if_supports_color(Stream::Stdout, |t| t.green())
            );
            return;
        }
        println!(
            "{} no issues found",
            "✓".if_supports_color(Stream::Stdout, |t| t.green())
        );
        return;
    }

    for f in &r.findings {
        let label = match f.severity {
            Severity::High => "high"
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
            Severity::Medium => "medium"
                .if_supports_color(Stream::Stdout, |t| t.yellow())
                .to_string(),
            Severity::Low => "low"
                .if_supports_color(Stream::Stdout, |t| t.blue())
                .to_string(),
        };
        let category = category_name(f.category)
            .if_supports_color(Stream::Stdout, |t| t.cyan())
            .to_string();
        let location = f
            .span
            .map(|s| format!("{}:{}: ", s.start.line, s.start.column))
            .unwrap_or_default();
        let rule = format!("[{}]", f.rule)
            .if_supports_color(Stream::Stdout, |t| t.dimmed())
            .to_string();
        let title = f.title.if_supports_color(Stream::Stdout, |t| t.bold());
        println!("{prefix}{location}{label} · {category} · {title}  {rule}");
        println!("    {}", f.what);
        if !f.why.is_empty() {
            println!(
                "    {}",
                f.why.if_supports_color(Stream::Stdout, |t| t.dimmed())
            );
        }
        for rem in &f.remedies {
            print_remedy(rem);
        }
    }

    print_hotspots(r);

    if r.advice.iter().any(|a| a.hypothetical) {
        eprintln!(
            "{}",
            "potential advice (no schema provided — verify the column isn't already indexed)"
                .if_supports_color(Stream::Stderr, |t| t.dimmed())
        );
    }
    for a in &r.advice {
        let label = if a.hypothetical {
            "potential advice"
        } else {
            "advice"
        };
        let header = label.if_supports_color(Stream::Stdout, |t| t.cyan());
        let location = a
            .span
            .map(|s| format!("{}:{}: ", s.start.line, s.start.column))
            .unwrap_or_default();
        println!("{location}{header} [{}]", a.subject);
        for rem in &a.remedies {
            print_remedy(rem);
        }
    }

    let n = r.findings.len();
    let plural = if n == 1 { "" } else { "s" };
    let a = r.advice.len();
    let advisories = if a == 1 { "advisory" } else { "advisories" };
    eprintln!("{n} finding{plural}, {a} {advisories}");
}

/// A banner when the query is a parameterized template: it isn't executable as written, and
/// (with a schema) what type each parameter expects.
fn print_parameters(r: &RenderedResult) {
    if r.parameters.is_empty() {
        return;
    }
    let n = r.parameters.len();
    let plural = if n == 1 { "" } else { "s" };
    let header = "parameterized query".if_supports_color(Stream::Stdout, |t| t.cyan());
    println!(
        "{header} · {n} parameter{plural} — bind each before running; not executable as written"
    );
    for p in &r.parameters {
        let ty = p.ty.as_deref().unwrap_or("type unknown");
        let uses = if p.spans.len() > 1 {
            format!("  ×{}", p.spans.len())
        } else {
            String::new()
        };
        println!(
            "    {}   {}{}",
            p.name,
            ty.if_supports_color(Stream::Stdout, |t| t.dimmed()),
            uses.if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
    }
}

/// The plan's heaviest nodes — a ranked cost summary. Each line names the node and why it's heavy;
/// the fix lives in the cross-linked findings above, pointed to by rule id.
fn print_hotspots(r: &RenderedResult) {
    if r.hotspots.is_empty() {
        return;
    }
    let header =
        "performance hotspots (from the plan)".if_supports_color(Stream::Stdout, |t| t.cyan());
    println!("{header}");
    for h in &r.hotspots {
        let at = h
            .relation
            .as_ref()
            .map(|n| format!(" {n}"))
            .unwrap_or_default();
        let volume = match (h.rows.actual, h.rows.est) {
            (Some(a), _) => format!("  {a} rows"),
            (None, Some(e)) => format!("  ~{e} rows (est)"),
            _ => String::new(),
        };
        let time = h
            .time_ms
            .map(|t| {
                let workers = if h.worker_summed_time {
                    " summed across parallel workers"
                } else {
                    ""
                };
                format!("  {t:.1}ms{workers}")
            })
            .unwrap_or_default();
        println!(
            "    {}{at} · {}{}",
            node_kind_name(&h.kind),
            h.cause,
            format!("{volume}{time}").if_supports_color(Stream::Stdout, |t| t.dimmed())
        );
        if !h.linked_rules.is_empty() {
            println!(
                "      {}",
                format!("see: {}", h.linked_rules.join(", "))
                    .if_supports_color(Stream::Stdout, |t| t.dimmed())
            );
        }
    }
}

/// A human label for a plan node kind.
fn node_kind_name(kind: &varq_core_parse::plan::NodeKind) -> &str {
    use varq_core_parse::plan::NodeKind::*;
    match kind {
        Scan => "scan",
        NestedLoop => "nested loop",
        HashJoin => "hash join",
        MergeJoin => "merge join",
        Sort => "sort",
        Aggregate => "aggregate",
        Hash => "hash",
        Limit => "limit",
        Materialize => "materialize",
        Other(s) => s,
    }
}

/// One remedy, indented under its finding/advice.
fn print_remedy(rem: &varq_core_parse::enrich::Remedy) {
    let tag = match &rem.apply {
        Some(a) if a.changes_results => " (fix — changes results)",
        Some(_) => " (auto-fix)",
        None => "",
    };
    let title = format!("→ {}{tag}", rem.title);
    println!(
        "    {}",
        title.if_supports_color(Stream::Stdout, |t| t.green())
    );
    println!("      {}", rem.how_to_implement);
    if let Some(w) = &rem.when {
        println!("      when: {w}");
    }
    if let Some(ex) = &rem.example {
        println!(
            "      e.g. {}",
            ex.if_supports_color(Stream::Stdout, |t| t.cyan())
        );
    }
    if let Some(t) = &rem.tradeoff {
        println!("      tradeoff: {t}");
    }
}
