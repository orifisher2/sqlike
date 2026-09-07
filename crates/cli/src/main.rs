//! The `sqlike` command-line tool — a thin shell over `varq_core::analyze`.
//!
//! All analysis is pure and lives in `core`; this crate handles I/O, formatting,
//! and exit codes.

mod check;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use owo_colors::{OwoColorize, Stream};

use varq_client::{Confidence, EquivalenceVerdict, FacetVerdict, Overall, PropertyReport};
use varq_core_parse::Dialect;

#[derive(Parser)]
#[command(
    name = "sqlike",
    version,
    about = "Deterministic SQL static analyzer (Postgres, MySQL, SQLite, SQL Server, MariaDB, DuckDB)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The hosted backend. A remote-only build has no local engine, so this is where `check` and
/// `diff` go when the caller names no server — matching `crates/mcp`, which already defaults here.
const DEFAULT_URL: &str = "https://api.sqlike.com";

/// Resolve the server URL: `--remote` wins, then `SQLIKE_URL`, then (remote-only builds) the
/// hosted default. A `local` build keeps `None` meaning "analyze here", so it stays offline
/// unless asked otherwise.
pub(crate) fn resolve_remote(flag: Option<String>) -> Option<String> {
    let resolved = flag.or_else(|| std::env::var("SQLIKE_URL").ok().filter(|s| !s.is_empty()));
    #[cfg(not(feature = "local"))]
    let resolved = resolved.or_else(|| Some(DEFAULT_URL.to_string()));
    resolved
}

/// Resolve the API key: `--key` wins, then `SQLIKE_API_KEY` — the same name `crates/mcp` reads,
/// so one environment configures every client.
pub(crate) fn resolve_key(flag: Option<String>) -> Option<String> {
    flag.or_else(|| {
        std::env::var("SQLIKE_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// clap value parser for `--dialect`. Kept here (not a `ValueEnum` derive on
/// `varq_core_parse::Dialect`) so `core-parse` stays free of a `clap` dependency.
fn parse_dialect(s: &str) -> Result<Dialect, String> {
    match s {
        "postgres" => Ok(Dialect::Postgres),
        "mysql" => Ok(Dialect::Mysql),
        "sqlite" => Ok(Dialect::Sqlite),
        "mssql" => Ok(Dialect::Mssql),
        "mariadb" => Ok(Dialect::Mariadb),
        "duckdb" => Ok(Dialect::Duckdb),
        other => Err(format!(
            "unknown dialect `{other}` (expected postgres, mysql, sqlite, mssql, mariadb, or \
             duckdb)"
        )),
    }
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
        /// SQL files to analyze, or `-` for stdin. Several may be given: a pre-commit hook
        /// passes the staged files, and a CI job passes the ones a pull request changed.
        #[arg(required = true, num_args = 1..)]
        query: Vec<PathBuf>,
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
        /// Analyze on a remote sqlike server (base URL). Defaults to `SQLIKE_URL`, then the
        /// hosted API — a build without the local engine always has a server to talk to.
        #[arg(long)]
        remote: Option<String>,
        /// API key for the remote server (sent as a Bearer token). Defaults to `SQLIKE_API_KEY`.
        #[arg(long)]
        key: Option<String>,
        /// Fail the process on `never`, `warn`, or `block` (the default). `block` is a real
        /// defect — invalid SQL or a high-severity correctness problem; advisory findings pass.
        #[arg(long, value_parser = check::parse_fail_on, default_value = "block")]
        fail_on: check::FailOn,
        /// Allow sending the raw query when it can't be parsed (and so can't be tokenized before
        /// leaving the machine). Off by default: an unparseable query is refused, not sent raw.
        #[arg(long)]
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
        #[arg(long, default_value = DEFAULT_URL)]
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

/// Exit codes are a contract with CI, so each one means exactly one thing: 0 clean (or below
/// `--fail-on`), 1 warn, 2 block, 3 operational failure (unreachable server, rate limit,
/// unreadable file), 4 usage error. 2 used to double as "something went wrong", which made an
/// outage indistinguishable from dangerous SQL.
fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // `--help`/`--version` arrive here as errors that print to stdout and are not failures.
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                ExitCode::from(4)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    varq_client::set_client(varq_client::Client::Cli);
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!(
                "{}: {e:#}",
                "error".if_supports_color(Stream::Stderr, |t| t.red())
            );
            ExitCode::from(3)
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
            fail_on,
            allow_raw,
        } => check::run_check(
            query, schema, stats, explain, dialect, sort, json, remote, key, fail_on, allow_raw,
        ),
        Command::Diff {
            old,
            new,
            schema,
            dialect,
            remote,
            key,
            fail_on,
        } => run_diff(
            &old,
            &new,
            schema.as_deref(),
            dialect,
            &remote,
            resolve_key(key).as_deref(),
            &fail_on,
        ),
    }
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
            Confidence::Bounded => "bounded",
            Confidence::Formal => "formal",
        };
        println!("confidence: {cl}");
    }
}

fn print_facet(name: &str, f: &FacetVerdict) {
    // A counterexample database is multi-line, so it gets its own indented block instead of being
    // flattened into the parenthesis — the rows are the evidence and have to stay readable.
    if let FacetVerdict::Differ { detail, .. } = f {
        if detail.contains('\n') {
            println!("  {name}: differ");
            for line in detail.lines() {
                println!("    {}", line.trim_end());
            }
            return;
        }
    }
    let state = match f {
        FacetVerdict::Match { .. } => "match".to_string(),
        FacetVerdict::Differ { detail, .. } => format!("differ ({detail})"),
        FacetVerdict::Undecided { reason } => format!("undecided ({reason})"),
        FacetVerdict::NotApplicable => "n/a".to_string(),
    };
    println!("  {name}: {state}");
}

/// Read the query from a file, or from stdin when the path is `-`.
pub(crate) fn read_input(path: &Path) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use varq_client::ColumnFacets;

    fn m() -> FacetVerdict {
        FacetVerdict::matched(Confidence::Structural)
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
