//! The `sqlike` command-line tool — a thin shell over `varq_core::analyze`.
//!
//! All analysis is pure and lives in `core`; this crate handles I/O, formatting,
//! and exit codes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream};

use varq_client::{Confidence, EquivalenceVerdict, FacetVerdict, Overall, PropertyReport};
use varq_core_parse::enrich::{FindingSort, RenderedResult};
#[cfg(feature = "local")]
use varq_core_parse::plan::Plan;
use varq_core_parse::result::{Category, Outcome, Severity};
#[cfg(feature = "local")]
use varq_core_parse::schema::Stats;
use varq_core_parse::Dialect;
// The local analysis engine is the only `varq-core` reference, gated behind the `local` feature.
// A remote-only build (`--no-default-features`) doesn't link `core` at all — the distributable,
// publishable client, a pure forwarder. All types above come from the public `core-parse`.
#[cfg(feature = "local")]
use varq_core::analyze_with_plan;

#[derive(Parser)]
#[command(
    name = "sqlike",
    version,
    about = "Deterministic SQL static analyzer (Postgres, MySQL, SQLite, SQL Server)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// clap value parser for `--dialect`. Kept here (not a `ValueEnum` derive on
/// `varq_core_parse::Dialect`) so `core-parse` stays free of a `clap` dependency.
fn parse_dialect(s: &str) -> Result<Dialect, String> {
    match s {
        "postgres" => Ok(Dialect::Postgres),
        "mysql" => Ok(Dialect::Mysql),
        "sqlite" => Ok(Dialect::Sqlite),
        "mssql" => Ok(Dialect::Mssql),
        other => Err(format!(
            "unknown dialect `{other}` (expected postgres, mysql, sqlite, or mssql)"
        )),
    }
}

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

/// A contract facet for `diff --fail-on`: a difference here is a *note* (not a data change), and
/// selecting it promotes that note to a failure (exit 1).
#[derive(Clone, Copy)]
enum NoteFacet {
    Names,
    Position,
    Order,
}

fn parse_note_facet(s: &str) -> Result<NoteFacet, String> {
    match s {
        "names" => Ok(NoteFacet::Names),
        "position" => Ok(NoteFacet::Position),
        "order" => Ok(NoteFacet::Order),
        other => Err(format!(
            "unknown facet `{other}` (expected names, position, or order)"
        )),
    }
}

#[derive(Subcommand)]
enum Command {
    /// Analyze a SQL query.
    Check {
        /// SQL file to analyze, or `-` for stdin.
        query: PathBuf,
        /// Schema DDL file (CREATE TABLE / CREATE INDEX) for schema-aware checks.
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Table row-count estimates as a JSON map (e.g. `{"orders": 2000000}`) so index advice
        /// is volume-aware. Local analysis only for now.
        #[arg(long)]
        stats: Option<PathBuf>,
        /// A query plan to sharpen the missing-index findings (confirm or suppress them from what
        /// the planner actually did): Postgres `EXPLAIN (FORMAT JSON)`, MySQL `EXPLAIN
        /// FORMAT=JSON`, SQLite `EXPLAIN QUERY PLAN` (`.mode json` rows), or SQL Server
        /// `SHOWPLAN_XML`. Tokenized before it's sent remote.
        #[arg(long)]
        explain: Option<PathBuf>,
        /// SQL dialect to analyze under.
        #[arg(long, value_parser = parse_dialect, default_value = "postgres")]
        dialect: Dialect,
        /// Order issues by a comma-separated key list (each breaks ties of the previous):
        /// any of `severity`, `type`, `location`.
        #[arg(long, default_value = "severity,type,location")]
        sort: String,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
        /// Analyze on a remote sqlike server (base URL) instead of locally.
        #[arg(long)]
        remote: Option<String>,
        /// API key for the remote server (sent as a Bearer token).
        #[arg(long, requires = "remote")]
        key: Option<String>,
        /// Allow sending the raw query when it can't be parsed (and so can't be tokenized before
        /// leaving the machine). Off by default: an unparseable query is refused, not sent raw.
        #[arg(long, requires = "remote")]
        allow_raw: bool,
    },

    /// Check whether two queries are equivalent. Runs server-side — the engine never ships in
    /// the CLI, so this always forwards to a sqlike server (both queries tokenized first).
    Diff {
        /// The original query file.
        old: PathBuf,
        /// The rewritten query to compare against.
        new: PathBuf,
        /// Schema DDL both queries resolve against (one schema — comparing over different
        /// schemas is ill-posed).
        #[arg(long)]
        schema: Option<PathBuf>,
        /// SQL dialect both queries are written in.
        #[arg(long, value_parser = parse_dialect, default_value = "postgres")]
        dialect: Dialect,
        /// sqlike server base URL (equivalence runs server-side).
        #[arg(long, default_value = "https://api.sqlike.com")]
        remote: String,
        /// API key for the remote server (sent as a Bearer token).
        #[arg(long)]
        key: Option<String>,
        /// Treat these note facets as failures (exit 1): comma-separated, any of
        /// `names`, `position`, `order`.
        #[arg(long, value_delimiter = ',', value_parser = parse_note_facet)]
        fail_on: Vec<NoteFacet>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!(
                "{}: {e:#}",
                "error".if_supports_color(Stream::Stderr, |t| t.red())
            );
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Check {
            query,
            schema,
            stats,
            explain,
            dialect,
            sort,
            json,
            remote,
            key,
            allow_raw,
        } => run_check(
            query, schema, stats, explain, dialect, sort, json, remote, key, allow_raw,
        ),
        Command::Diff {
            old,
            new,
            schema,
            dialect,
            remote,
            key,
            fail_on,
        } => match run_diff(
            &old,
            &new,
            schema.as_deref(),
            dialect,
            &remote,
            key.as_deref(),
            &fail_on,
        ) {
            Ok(code) => Ok(code),
            // Operational failure (bad file, transport error, a query that didn't parse) is not a
            // verdict — exit 3, distinct from the verdict codes 0/1/2.
            Err(e) => {
                eprintln!(
                    "{}: {e:#}",
                    "error".if_supports_color(Stream::Stderr, |t| t.red())
                );
                Ok(ExitCode::from(3))
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn run_check(
    query: PathBuf,
    schema: Option<PathBuf>,
    stats: Option<PathBuf>,
    explain: Option<PathBuf>,
    dialect: Dialect,
    sort: String,
    json: bool,
    remote: Option<String>,
    key: Option<String>,
    allow_raw: bool,
) -> Result<ExitCode> {
    let sort_keys = parse_sort_keys(&sort)?;
    let sql = read_input(&query)?;
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

    let mut result = match remote {
        Some(url) => {
            let r = varq_client::analyze(
                &url,
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
        None => analyze_with_plan(&sql, schema_ddl.as_deref(), stats.as_ref(), plan.as_ref(), dialect)
            .rendered(),
        #[cfg(not(feature = "local"))]
        None => anyhow::bail!(
            "this is a remote-only build — pass --remote <url>, e.g. --remote https://api.sqlike.com"
        ),
    };
    result.sort(&sort_keys);

    if json {
        println!("{}", result.to_json());
    } else {
        print_human(&result);
    }
    Ok(exit_code(&result))
}

/// Run `sqlike diff`: compare two queries server-side and map the verdict to an exit code.
fn run_diff(
    old: &Path,
    new: &Path,
    schema: Option<&Path>,
    dialect: Dialect,
    remote: &str,
    key: Option<&str>,
    fail_on: &[NoteFacet],
) -> Result<ExitCode> {
    let sql_a = read_input(old)?;
    let sql_b = read_input(new)?;
    let schema_ddl = schema
        .map(|p| {
            std::fs::read_to_string(p).with_context(|| format!("reading schema {}", p.display()))
        })
        .transpose()?;
    let verdict = varq_client::diff(remote, key, &sql_a, &sql_b, schema_ddl.as_deref(), dialect)?;
    print_verdict(&verdict);
    Ok(ExitCode::from(diff_exit_code(&verdict, fail_on)))
}

/// The `04b` exit-code contract: 0 = equivalent (or notes the caller tolerates), 1 = differs (or a
/// `--fail-on` note fired), 2 = undecided. Operational failure (exit 3) is handled by the caller,
/// never here — this maps a *verdict* only. Pure, so the whole contract is unit-tested below.
fn diff_exit_code(v: &EquivalenceVerdict, fail_on: &[NoteFacet]) -> u8 {
    match v.overall {
        Overall::Undecided => 2,
        Overall::Differs => 1,
        Overall::EquivalentWithNotes => {
            if fail_on.iter().any(|f| is_note(facet_of(&v.facets, *f))) {
                1
            } else {
                0
            }
        }
        Overall::Equivalent => 0,
    }
}

/// A contract facet is a *note* when it differs or couldn't be decided.
fn is_note(f: &FacetVerdict) -> bool {
    matches!(
        f,
        FacetVerdict::Differ { .. } | FacetVerdict::Undecided { .. }
    )
}

fn facet_of(r: &PropertyReport, f: NoteFacet) -> &FacetVerdict {
    match f {
        NoteFacet::Names => &r.columns.names,
        NoteFacet::Position => &r.columns.position,
        NoteFacet::Order => &r.order,
    }
}

fn print_verdict(v: &EquivalenceVerdict) {
    let label = match v.overall {
        Overall::Equivalent => "equivalent"
            .if_supports_color(Stream::Stdout, |t| t.green())
            .to_string(),
        Overall::EquivalentWithNotes => "equivalent (with notes)"
            .if_supports_color(Stream::Stdout, |t| t.yellow())
            .to_string(),
        Overall::Differs => "not equivalent"
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string(),
        Overall::Undecided => "undecided"
            .if_supports_color(Stream::Stdout, |t| t.blue())
            .to_string(),
    };
    println!("{label}");
    let f = &v.facets;
    print_facet("columns.arity", &f.columns.arity);
    print_facet("columns.names", &f.columns.names);
    print_facet("columns.types", &f.columns.types);
    print_facet("columns.position", &f.columns.position);
    print_facet("rows", &f.rows);
    print_facet("cardinality", &f.cardinality);
    print_facet("order", &f.order);
    if let Some(c) = v.confidence {
        let cl = match c {
            Confidence::Structural => "structural",
            Confidence::Empirical => "empirical",
            Confidence::Formal => "formal",
        };
        println!("confidence: {cl}");
    }
}

fn print_facet(name: &str, f: &FacetVerdict) {
    let state = match f {
        FacetVerdict::Match { .. } => "match".to_string(),
        FacetVerdict::Differ { detail, .. } => format!("differ ({detail})"),
        FacetVerdict::Undecided { reason } => format!("undecided ({reason})"),
        FacetVerdict::NotApplicable => "n/a".to_string(),
    };
    println!("  {name}: {state}");
}

/// Read the query from a file, or from stdin when the path is `-`.
fn read_input(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading query from stdin")?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}

/// Exit code from the category-aware policy in `core`: a broken/wrong query blocks (2),
/// advisories warn (1), clean passes (0).
fn exit_code(result: &RenderedResult) -> ExitCode {
    match result.outcome() {
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

fn print_human(r: &RenderedResult) {
    print_parameters(r);

    if r.findings.is_empty() && r.advice.is_empty() {
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
        println!("{location}{label} · {category} · {title}  {rule}");
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

/// One remedy, indented under its finding/advice.
fn print_remedy(rem: &varq_core_parse::enrich::Remedy) {
    let tag = if rem.apply.is_some() {
        " (auto-fix)"
    } else {
        ""
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

#[cfg(test)]
mod tests {
    use super::*;
    use varq_client::ColumnFacets;

    fn m() -> FacetVerdict {
        FacetVerdict::Match {
            by: Confidence::Structural,
        }
    }
    fn differ() -> FacetVerdict {
        FacetVerdict::Differ {
            detail: "x".into(),
            by: Confidence::Structural,
        }
    }
    fn uniform(v: FacetVerdict) -> PropertyReport {
        PropertyReport {
            columns: ColumnFacets {
                arity: v.clone(),
                names: v.clone(),
                types: v.clone(),
                position: v.clone(),
            },
            rows: v.clone(),
            cardinality: v.clone(),
            order: v,
        }
    }
    fn verdict(r: PropertyReport) -> EquivalenceVerdict {
        EquivalenceVerdict::from_facets(r)
    }

    #[test]
    fn equivalent_is_0() {
        assert_eq!(diff_exit_code(&verdict(uniform(m())), &[]), 0);
    }

    #[test]
    fn differs_is_1() {
        let mut r = uniform(m());
        r.rows = differ();
        assert_eq!(diff_exit_code(&verdict(r), &[]), 1);
    }

    #[test]
    fn undecided_is_2() {
        let mut r = uniform(m());
        r.rows = FacetVerdict::Undecided { reason: "x".into() };
        assert_eq!(diff_exit_code(&verdict(r), &[]), 2);
    }

    #[test]
    fn notes_pass_by_default() {
        let mut r = uniform(m());
        r.columns.names = differ(); // → EquivalentWithNotes
        assert_eq!(diff_exit_code(&verdict(r), &[]), 0);
    }

    #[test]
    fn fail_on_promotes_the_matching_note() {
        let mut r = uniform(m());
        r.columns.names = differ();
        assert_eq!(diff_exit_code(&verdict(r), &[NoteFacet::Names]), 1);
    }

    #[test]
    fn fail_on_ignores_a_nonmatching_note() {
        let mut r = uniform(m());
        r.order = differ(); // an order note, but we fail only on names
        assert_eq!(diff_exit_code(&verdict(r), &[NoteFacet::Names]), 0);
    }
}
