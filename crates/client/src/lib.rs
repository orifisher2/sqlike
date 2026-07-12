//! VARQ remote client: the shared path every front door (CLI, web, MCP) uses to
//! reach a VARQ server's `POST /v1/analyze` and decode its envelope into a
//! `RenderedResult`, so remote results render exactly like local ones.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use varq_core_parse::enrich::RenderedResult;
use varq_core_parse::tokenize::{
    finalize as finalize_envelope, tokenize, tokenize_pair, tokenize_with_stats, TokenMap,
};
use varq_core_parse::Dialect;

// The equivalence verdict types the CLI/agents deserialize — re-exported so a caller depends only
// on this crate, not `core-parse` directly. The *engine* that computes a verdict stays server-only.
pub use varq_core_parse::equivalence::{
    ColumnFacets, Confidence, EquivalenceVerdict, FacetVerdict, Overall, PropertyReport,
};

/// Wire-protocol version. The server ignores unknown fields, so bumping this stays additive.
const PROTOCOL: u8 = 1;

/// [`analyze`] returns this when the query can't be tokenized (it doesn't parse) and `allow_raw`
/// was not set — so the raw SQL was **not** sent. Callers detect it (downcast) to ask the user
/// before retrying with `allow_raw`, keeping the raw-send decision with the human.
#[derive(Debug)]
pub struct RawSendBlocked;

impl std::fmt::Display for RawSendBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this query did not parse, so it can't be tokenized — analyzing it would send the raw \
             SQL to the server. Enable raw sending to proceed, or fix the SQL so it parses.",
        )
    }
}

impl std::error::Error for RawSendBlocked {}

#[derive(Serialize)]
struct Request<'a> {
    sql: &'a str,
    schema: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stats: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    explain: Option<&'a str>,
    dialect: Dialect,
    tokenized: bool,
    protocol: u8,
}

/// Analyze `sql` (and optional `schema`) on the server at `url` under `dialect`, returning the
/// same [`RenderedResult`] a local `analyze_with(...).rendered()` would — so the caller's
/// rendering is unchanged. `version`/`summary` in the envelope are server-derived; we ignore them
/// (serde drops unknown fields) and re-emit our own for `--json`.
///
/// The query and schema are **tokenized locally** before they leave the machine (v0.3): the
/// backend sees only opaque structure, and the response is detokenized here with a map that
/// never leaves the process. Unparseable SQL can't be tokenized; only if `allow_raw` is set does
/// it fall back to a **raw** request (for the server's parse-error finding). Otherwise the raw
/// SQL is not sent and [`RawSendBlocked`] is returned — the caller asks the user first.
#[allow(clippy::too_many_arguments)]
pub fn analyze(
    url: &str,
    key: Option<&str>,
    sql: &str,
    schema: Option<&str>,
    stats: Option<&str>,
    explain: Option<&str>,
    dialect: Dialect,
    allow_raw: bool,
) -> Result<RenderedResult> {
    let Ok(tok) = tokenize_with_stats(sql, schema, stats, explain, dialect) else {
        if !allow_raw {
            return Err(RawSendBlocked.into());
        }
        // Opted in: unparseable SQL → raw request; the stats (real names) match the raw query's
        // real names. A plan can't sharpen a parse-error finding, so it's dropped on this path.
        let text = post(url, key, sql, schema, stats, None, dialect, false)?;
        return decode(&text);
    };
    let text = post(
        url,
        key,
        &tok.payload,
        tok.schema_payload.as_deref(),
        tok.stats_payload.as_deref(),
        tok.explain_payload.as_deref(),
        dialect,
        true,
    )?;
    finalize(&text, &tok.map, sql, schema, dialect)
}

/// POST the request body and return the response text, surfacing the server's `{ "error" }`.
#[allow(clippy::too_many_arguments)]
fn post(
    url: &str,
    key: Option<&str>,
    sql: &str,
    schema: Option<&str>,
    stats: Option<&str>,
    explain: Option<&str>,
    dialect: Dialect,
    tokenized: bool,
) -> Result<String> {
    let endpoint = format!("{}/v1/analyze", url.trim_end_matches('/'));
    let body = serde_json::to_string(&Request {
        sql,
        schema,
        stats,
        explain,
        dialect,
        tokenized,
        protocol: PROTOCOL,
    })
    .expect("request serializes");
    send(&endpoint, key, &body, url)
}

/// POST `body` to `endpoint` and return the response text, surfacing the server's `{ "error" }`.
fn send(endpoint: &str, key: Option<&str>, body: &str, url: &str) -> Result<String> {
    // Don't turn a 4xx/5xx into a transport error — we want to read the server's
    // `{ "error": … }` body and surface its message.
    let mut req = ureq::post(endpoint)
        .config()
        .http_status_as_error(false)
        .build()
        .header("Content-Type", "application/json");
    if let Some(k) = key {
        req = req.header("Authorization", &format!("Bearer {k}"));
    }

    let mut resp = req
        .send(body)
        .with_context(|| format!("could not reach sqlike server at {url}"))?;

    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .context("reading server response")?;

    if status != 200 {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or(text);
        bail!("server returned {status}: {msg}");
    }
    Ok(text)
}

#[derive(Serialize)]
struct DiffRequest<'a> {
    sql_a: &'a str,
    sql_b: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a str>,
    dialect: Dialect,
    tokenized: bool,
    protocol: u8,
}

/// Compare two queries on the server at `url`, returning the per-property [`EquivalenceVerdict`].
///
/// Both queries are tokenized under **one shared map** before they leave the machine (so a table
/// named in both lines up, and the backend sees only opaque structure). A query that doesn't parse
/// **can't be compared** — that's invalid input, surfaced as an error, not a `Undecided` verdict
/// and not a raw fallback (equivalence has no parse-*finding* to return, unlike `analyze`).
pub fn diff(
    url: &str,
    key: Option<&str>,
    sql_a: &str,
    sql_b: &str,
    schema: Option<&str>,
    dialect: Dialect,
) -> Result<EquivalenceVerdict> {
    let pair = tokenize_pair(sql_a, sql_b, schema, dialect).map_err(|_| {
        let culprit = if tokenize(sql_a, schema, dialect).is_err() {
            "the first query"
        } else if tokenize(sql_b, schema, dialect).is_err() {
            "the second query"
        } else {
            "the schema"
        };
        anyhow!("cannot compare: {culprit} did not parse")
    })?;
    let endpoint = format!("{}/v1/equivalence", url.trim_end_matches('/'));
    let body = serde_json::to_string(&DiffRequest {
        sql_a: &pair.payload_a,
        sql_b: &pair.payload_b,
        schema: pair.schema_payload.as_deref(),
        dialect,
        tokenized: true,
        protocol: PROTOCOL,
    })
    .expect("request serializes");
    // Tier-1 verdict details are generic (no token text), so no detokenization is needed yet; a
    // future token-bearing detail would be mapped back through `pair`'s token map here.
    let text = send(&endpoint, key, &body, url)?;
    serde_json::from_str(&text).context("decoding equivalence verdict")
}

/// Decode a raw (non-tokenized) envelope straight into a `RenderedResult` (unknown
/// `version`/`summary` fields are ignored).
fn decode(envelope_json: &str) -> Result<RenderedResult> {
    serde_json::from_str(envelope_json).context("decoding server response")
}

/// Detokenize a tokenized envelope and rebuild the typo suggestions the server couldn't
/// (Decision 6), via the shared `core-parse` finalize so CLI and web behave identically.
fn finalize(
    envelope_json: &str,
    map: &TokenMap,
    sql: &str,
    schema: Option<&str>,
    dialect: Dialect,
) -> Result<RenderedResult> {
    let json = finalize_envelope(envelope_json, map, sql, schema, dialect)
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .context("finalizing server response")?;
    decode(&json)
}

