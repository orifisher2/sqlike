//! sqlike-mcp: an MCP stdio server exposing two tools — `analyze` (single-query static analysis)
//! and `diff` (whether two queries are equivalent) — each forwarding to the SQLike backend via
//! `crates/client` and returning the JSON envelope.
//!
//! The engines run **server-side** — this binary never calls `varq_core::analyze` or the
//! equivalence engine; it only forwards (the `forwards_never_analyzes` test guards that). It
//! links only the public `core-parse` crate (for the `Dialect` value type), never the closed engine.
//!
//! Config: `SQLIKE_URL` (default the hosted backend), `SQLIKE_API_KEY` (optional → Bearer).

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use varq_core_parse::Dialect;

const DEFAULT_URL: &str = "https://api.sqlike.com";

#[derive(Debug, Deserialize, JsonSchema)]
struct AnalyzeArgs {
    /// The SQL query to analyze.
    sql: String,
    /// Optional schema DDL (CREATE TABLE / CREATE INDEX) for column- and type-aware checks.
    #[serde(default)]
    schema: Option<String>,
    /// Optional table row counts as a JSON map, e.g. `{"orders": 2000000}`. Severity scales with
    /// volume: a large table promotes a performance finding, a small one demotes it and drops
    /// index advice. Table names are tokenized before it is sent; the numbers travel as numbers.
    #[serde(default)]
    stats: Option<String>,
    /// Optional query plan, to confirm or dismiss missing-index findings by what the planner
    /// actually did and, with actual row counts, re-score every performance finding: Postgres
    /// `EXPLAIN (ANALYZE, FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`, SQLite `EXPLAIN QUERY PLAN`
    /// (`.mode json` rows), SQL Server `SHOWPLAN_XML` or `STATISTICS XML`, DuckDB `EXPLAIN
    /// (FORMAT JSON)`. Identifiers are tokenized before it is sent.
    #[serde(default)]
    explain: Option<String>,
    /// SQL dialect: "postgres" (default), "mysql", "sqlite", "mssql", "mariadb", or "duckdb".
    #[serde(default)]
    dialect: Option<String>,
    /// Only matters when the query can't be tokenized/privacy-masked — it didn't parse locally, or
    /// it holds a name that can't be masked. Set true to send the RAW SQL to the server anyway.
    /// Default false blocks that — ask the user before setting it.
    #[serde(default)]
    allow_raw: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiffArgs {
    /// The original query.
    sql_a: String,
    /// The rewritten query to check for equivalence against `sql_a`.
    sql_b: String,
    /// Optional schema DDL both queries resolve against (one schema — comparing over different
    /// schemas is ill-posed).
    #[serde(default)]
    schema: Option<String>,
    /// SQL dialect: "postgres" (default), "mysql", "sqlite", "mssql", "mariadb", or "duckdb".
    #[serde(default)]
    dialect: Option<String>,
}

/// An absent dialect defaults to Postgres; an *unrecognized* one is an error. It used to fall
/// through to Postgres, which silently answered for the wrong engine — a caller that misspells
/// `mariadb` should be told, not handed Postgres verdicts.
fn dialect_of(d: Option<&str>) -> Result<Dialect, ErrorData> {
    match d {
        None | Some("postgres") => Ok(Dialect::Postgres),
        Some("mysql") => Ok(Dialect::Mysql),
        Some("sqlite") => Ok(Dialect::Sqlite),
        Some("mssql") => Ok(Dialect::Mssql),
        Some("mariadb") => Ok(Dialect::Mariadb),
        Some("duckdb") => Ok(Dialect::Duckdb),
        Some(other) => Err(ErrorData::invalid_params(
            format!(
                "unknown dialect `{other}` (expected postgres, mysql, sqlite, mssql, mariadb, or \
                 duckdb)"
            ),
            None,
        )),
    }
}

#[derive(Clone)]
struct Varq {
    // Consumed by the `#[tool_handler]`-generated `ServerHandler` methods to route calls;
    // rustc's dead-code pass can't see that macro-internal read, hence the allow.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    url: String,
    key: Option<String>,
}

#[tool_router]
impl Varq {
    fn new(url: String, key: Option<String>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            url,
            key,
        }
    }

    #[tool(
        description = "Use when you write, edit, or review a SQL query and want it checked before it runs. Returns SQLike's deterministic analysis as a JSON envelope: validity errors, anti-patterns, safe rewrites, and schema/index advice. Pass optional schema DDL for column- and type-aware checks, table row counts (stats) so severity follows volume, an EXPLAIN plan so index findings are confirmed or dismissed by what the planner did, and dialect (postgres default, mysql, mariadb, sqlite, mssql, duckdb — DuckDB is columnar, so its severities and index advice differ). The query is tokenized locally before it leaves the machine — identifiers and literals are masked. A query that does not parse is answered locally, with a parse-error finding that names the mistake, or names the construct SQLike does not support yet and says the query is valid, and sends nothing. A query holding a name that can't be masked makes the tool refuse rather than send raw SQL; on that refusal, ask the user whether to send the raw query, and only then call again with allow_raw=true"
    )]
    async fn analyze(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let dialect = dialect_of(args.dialect.as_deref())?;
        let (url, key) = (self.url.clone(), self.key.clone());

        // `varq_client::analyze` is blocking (ureq) — run it off the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            varq_client::analyze(
                &url,
                key.as_deref(),
                &args.sql,
                args.schema.as_deref(),
                args.stats.as_deref(),
                args.explain.as_deref(),
                dialect,
                args.allow_raw,
            )
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task join failed: {e}"), None))?;

        match result {
            Ok(r) => Ok(CallToolResult::success(vec![ContentBlock::text(
                r.to_json(),
            )])),
            // A consent gate, not a failure: the query can't be masked, so it can't be sent. Return
            // a plain result telling the agent to ask the user before retrying with allow_raw.
            Err(e) if e.downcast_ref::<varq_client::RawSendBlocked>().is_some() => {
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    "BLOCKED: this query holds a name SQLike cannot hide, and analyzing it would \
                     send the raw SQL off the user's machine. Ask the user whether to send the raw \
                     query; if they agree, call analyze again with allow_raw=true. (A query that \
                     does not parse is not this case: it is answered locally with a parse-error \
                     finding that says what is wrong.)",
                )]))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        description = "Use to confirm two SQL queries are equivalent — whenever you rewrite, refactor, or optimize a query and need to prove it still returns the same results (something an LLM cannot reliably self-grade). Returns SQLike's deterministic JSON verdict: an overall result (Equivalent / EquivalentWithNotes / Differs / Undecided / NotComparable), a confidence level, and a per-property report (columns, rows, cardinality, order). Undecided never means equivalent. If one query is invalid (a missing table or column, an ambiguous reference) the pair is Differs, since a query that does not run cannot return the same rows as one that does; if both are invalid it is NotComparable, with a reason for each. A query that does not parse is refused with the reason: the mistake, or the construct SQLike does not support yet. Both queries share one optional schema DDL; dialect is postgres default, mysql, mariadb, sqlite, mssql, duckdb."
    )]
    async fn diff(
        &self,
        Parameters(args): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let dialect = dialect_of(args.dialect.as_deref())?;
        let (url, key) = (self.url.clone(), self.key.clone());

        // `varq_client::diff` is blocking (ureq) — run it off the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            varq_client::diff(
                &url,
                key.as_deref(),
                &args.sql_a,
                &args.sql_b,
                args.schema.as_deref(),
                dialect,
            )
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task join failed: {e}"), None))?;

        match result {
            Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(
                v.to_json(),
            )])),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }
}

#[tool_handler]
impl ServerHandler for Varq {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info.name = "sqlike-mcp".into();
        // Without this, `ServerInfo::default()` leaves rmcp's own version here, so every
        // client that shows a server version showed the rmcp release instead of ours — and it
        // moved on its own when rmcp went 1 -> 3.
        info.server_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "SQLike — deterministic SQL static analysis and equivalence checking (no LLM in the analysis path). Reach for `analyze` whenever you produce or review SQL: pass a query (plus optional schema DDL, table row counts, an EXPLAIN plan, and dialect) to get validity, anti-patterns, rewrites, and schema/index advice. Reach for `diff` whenever you rewrite or refactor a query to check the new version is equivalent (result-preserving) — a verdict an LLM cannot reliably self-grade. Queries are tokenized locally before leaving the machine; an unparseable query is refused rather than sent raw. How to read the results: a high-severity or validity finding is a must-fix; a finding that carries a rewrite should be applied as given, then confirmed with `diff`. A `diff` verdict of Undecided never means equivalent: say the rewrite is unverified rather than claim it is safe. What to ask the user for: the query alone is usually enough, but if they say a query is slow in production, ask for the actual execution plan (Postgres `EXPLAIN (ANALYZE, FORMAT JSON)`, MySQL `EXPLAIN FORMAT=JSON`) and the table sizes before proposing a fix, and pass them as `explain` and `stats`, since without them findings are ranked by shape, not by cause; if they ask you to write a query against tables you have not seen, ask for the schema DDL and pass it as `schema`, since column and type checks are skipped without it. Everything sent is tokenized on the machine first, so production output is safe to paste. Do not block a simple task on extra data: analyze what you have and say which input would sharpen which finding."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    varq_client::set_client(varq_client::Client::Mcp);
    let url = std::env::var("SQLIKE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let key = std::env::var("SQLIKE_API_KEY").ok();

    // MCP speaks JSON-RPC on stdout — the transport owns it; we emit nothing else there.
    let service = Varq::new(url, key).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

