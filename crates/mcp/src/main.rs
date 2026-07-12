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
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
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
    /// SQL dialect: "postgres" (default), "mysql", "sqlite", or "mssql".
    #[serde(default)]
    dialect: Option<String>,
    /// Only matters when the query fails to parse (and so can't be tokenized/privacy-masked):
    /// set true to send the RAW SQL to the server for a parse diagnostic. Default false blocks
    /// that — ask the user before setting it.
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
    /// SQL dialect: "postgres" (default), "mysql", "sqlite", or "mssql".
    #[serde(default)]
    dialect: Option<String>,
}

fn dialect_of(d: Option<&str>) -> Dialect {
    match d {
        Some("mysql") => Dialect::Mysql,
        Some("sqlite") => Dialect::Sqlite,
        Some("mssql") => Dialect::Mssql,
        _ => Dialect::Postgres,
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
        description = "Analyze a SQL query with SQLike: validity, anti-patterns, suggested \
                       rewrites, and schema/index advice. Returns the JSON analysis envelope. \
                       Supports Postgres (default), MySQL, SQLite, and SQL Server. The query is \
                       tokenized locally before it leaves the machine — identifiers and literals \
                       are masked. A query that can't be parsed can't be tokenized; the tool then \
                       refuses rather than send raw SQL. If you get such a refusal, ask the user \
                       whether to send the raw query, and only then retry with allow_raw=true."
    )]
    async fn analyze(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let dialect = dialect_of(args.dialect.as_deref());
        let (url, key) = (self.url.clone(), self.key.clone());

        // `varq_client::analyze` is blocking (ureq) — run it off the async runtime.
        let result = tokio::task::spawn_blocking(move || {
            varq_client::analyze(
                &url,
                key.as_deref(),
                &args.sql,
                args.schema.as_deref(),
                None,
                None,
                dialect,
                args.allow_raw,
            )
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("task join failed: {e}"), None))?;

        match result {
            Ok(r) => Ok(CallToolResult::success(vec![Content::text(r.to_json())])),
            // A consent gate, not a failure: the query didn't parse, so it can't be masked. Return
            // a plain result telling the agent to ask the user before retrying with allow_raw.
            Err(e) if e.downcast_ref::<varq_client::RawSendBlocked>().is_some() => {
                Ok(CallToolResult::success(vec![Content::text(
                    "BLOCKED: this query did not parse, so SQLike can't tokenize it, and analyzing \
                     it would send the raw SQL off the user's machine. Ask the user whether to send \
                     the raw query; if they agree, call analyze again with allow_raw=true. \
                     Otherwise fix the SQL so it parses.",
                )]))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        description = "Check whether two SQL queries are equivalent with SQLike — for verifying a \
                       rewrite/refactor preserves results. Returns the JSON verdict: an overall \
                       result (Equivalent / EquivalentWithNotes / Differs / Undecided), a \
                       confidence level, and a per-property report (columns, rows, cardinality, \
                       order). Undecided never means equivalent. Both queries share one optional \
                       schema. Supports Postgres (default), MySQL, SQLite, and SQL Server."
    )]
    async fn diff(
        &self,
        Parameters(args): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let dialect = dialect_of(args.dialect.as_deref());
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
            Ok(v) => Ok(CallToolResult::success(vec![Content::text(v.to_json())])),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }
}

#[tool_handler]
impl ServerHandler for Varq {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.server_info.name = "sqlike-mcp".into();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "SQLike — deterministic SQL static analysis. Call `analyze` with a SQL query \
             (and optional schema DDL and dialect) to get anti-patterns, rewrites, and \
             schema advice. Call `diff` with two queries to check whether a rewrite is \
             equivalent (result-preserving) — a verdict an LLM cannot reliably self-grade."
                .into(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("SQLIKE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let key = std::env::var("SQLIKE_API_KEY").ok();

    // MCP speaks JSON-RPC on stdout — the transport owns it; we emit nothing else there.
    let service = Varq::new(url, key).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

