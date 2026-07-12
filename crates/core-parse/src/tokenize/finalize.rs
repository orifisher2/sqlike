//! Turn the server's tokenized response into the caller's real result: detokenize, then recompute
//! the typo suggestions the server couldn't (Decision 6 — it only saw opaque tokens, so every
//! reference looked alike). Shared by the native client and the WASM bundle so every front door
//! behaves identically.

use serde_json::Value;

use super::{detokenize, TokenMap, TokenizeError};
use crate::dialect::Dialect;
use crate::enrich::resolve_remedy;
use crate::model::resolve_suggestions;

/// Resolve-validity rules whose `suggestion` is recomputed here against the real names.
const RESOLVE_RULES: &[&str] = &[
    "unknown-column",
    "unknown-table",
    "ambiguous-column",
    "ambiguous-table",
    "unknown-table-alias",
];

/// Detokenize `envelope_json` through `map`, then rebuild each resolve finding's did-you-mean
/// remedy from the real-name suggestion computed locally from `sql`/`schema` (the server's was
/// over opaque tokens — Decision 6). Returns the finished envelope JSON.
pub fn finalize(
    envelope_json: &str,
    map: &TokenMap,
    sql: &str,
    schema: Option<&str>,
    dialect: Dialect,
) -> Result<String, TokenizeError> {
    let detok = detokenize(envelope_json, map)?;
    let mut v: Value =
        serde_json::from_str(&detok).map_err(|e| TokenizeError::Internal(e.to_string()))?;
    patch_resolve_remedies(&mut v, sql, schema, dialect);
    serde_json::to_string_pretty(&v).map_err(|e| TokenizeError::Internal(e.to_string()))
}

/// Replace each resolve finding's `remedies` with the did-you-mean remedy built from the real
/// nearest name (or none) — the server's, computed over tokens, is meaningless.
fn patch_resolve_remedies(v: &mut Value, sql: &str, schema: Option<&str>, dialect: Dialect) {
    let local = resolve_suggestions(sql, schema, dialect);
    let Some(findings) = v.get_mut("findings").and_then(Value::as_array_mut) else {
        return;
    };
    for f in findings {
        if !f
            .get("rule")
            .and_then(Value::as_str)
            .is_some_and(|r| RESOLVE_RULES.contains(&r))
        {
            continue;
        }
        let here = f
            .get("span")
            .and_then(|s| s.get("start"))
            .and_then(|p| Some((p.get("line")?.as_u64()?, p.get("column")?.as_u64()?)));
        let remedies = here
            .and_then(|(line, col)| {
                local
                    .iter()
                    .find(|(l, _)| l.line == line && l.column == col)
            })
            .and_then(|(_, s)| serde_json::to_value([resolve_remedy(s)]).ok())
            .unwrap_or_else(|| Value::Array(Vec::new()));
        f["remedies"] = remedies;
    }
}
