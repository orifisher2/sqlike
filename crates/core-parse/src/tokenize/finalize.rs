//! Turn the server's tokenized response into the caller's real result: detokenize, then recompute
//! the typo suggestions the server couldn't (Decision 6 — it only saw opaque tokens, so every
//! reference looked alike). Shared by the native client and the WASM bundle so every front door
//! behaves identically.

use serde_json::Value;

use super::{detokenize, Lines, TokenMap, TokenizeError};
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
    rebuild_fixes(&mut v, sql, &Lines::of(sql));
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
            .and_then(|(_, s)| serde_json::to_value([resolve_remedy(s)]).ok());
        // No suggestion: no `remedies` key, as the engine writes it (UR1, K).
        match (remedies, f.as_object_mut()) {
            (Some(r), Some(o)) => {
                o.insert("remedies".into(), r);
            }
            (None, Some(o)) => {
                o.remove("remedies");
            }
            _ => {}
        }
    }
}

/// Rebuild every `apply.fixed_sql` from the caller's SQL and the fix's edits, which detokenizing
/// has already mapped back to it. The server's text was written over the payload, which has no
/// comments, so a fix used to drop the user's (UR1, G). A fix without edits keeps the server's.
fn rebuild_fixes(v: &mut Value, sql: &str, lines: &Lines) {
    match v {
        Value::Object(o) => {
            let rebuilt = o
                .get_mut("apply")
                .and_then(Value::as_object_mut)
                .and_then(|apply| {
                    let fixed = applied(apply.get("edits"), sql, lines)?;
                    apply
                        .insert("fixed_sql".into(), Value::String(fixed.clone()))
                        .map(|server| (server, fixed))
                });
            // A remedy whose example is the whole fixed query shows the rebuilt one too.
            if let Some((server, fixed)) = rebuilt {
                if o.get("example") == Some(&server) {
                    o.insert("example".into(), Value::String(fixed));
                }
            }
            o.values_mut().for_each(|x| rebuild_fixes(x, sql, lines));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rebuild_fixes(x, sql, lines)),
        _ => {}
    }
}

/// `sql` with `edits` applied, last first so earlier positions hold; `None` without edits.
fn applied(edits: Option<&Value>, sql: &str, lines: &Lines) -> Option<String> {
    let at =
        |p: &Value| Some(lines.byte_at(sql, p.get("line")?.as_u64()?, p.get("column")?.as_u64()?));
    let mut spans: Vec<(usize, usize, &str)> = edits?
        .as_array()?
        .iter()
        .map(|e| {
            let range = e.get("range")?;
            Some((
                at(range.get("start")?)?,
                at(range.get("end")?)?,
                e.get("replacement")?.as_str()?,
            ))
        })
        .collect::<Option<_>>()?;
    if spans.is_empty() {
        return None;
    }
    spans.sort_by_key(|&(start, ..)| std::cmp::Reverse(start));
    let mut out = sql.to_string();
    for (start, end, replacement) in spans {
        out.replace_range(start..end, replacement);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rebuilt fix keeps the caller's comment, which the payload the server saw did not have,
    /// and an example that was the whole fixed query follows it (UR1, G).
    #[test]
    fn a_fix_is_rebuilt_from_the_callers_sql() {
        let sql = "-- monthly report\nSELECT * FROM t WHERE a = 1 OR a = 2";
        let server_fix = "\nSELECT * FROM t WHERE a IN (1, 2)";
        let range = r#"{"start":{"line":2,"column":23},"end":{"line":2,"column":37}}"#;
        let mut v: Value = serde_json::from_str(&format!(
            r#"{{"remedies":[{{"example":{server_fix:?},
                "apply":{{"fixed_sql":{server_fix:?},
                "edits":[{{"range":{range},"replacement":"a IN (1, 2)"}}]}}}}]}}"#
        ))
        .unwrap();
        rebuild_fixes(&mut v, sql, &Lines::of(sql));
        let fixed = "-- monthly report\nSELECT * FROM t WHERE a IN (1, 2)";
        assert_eq!(v["remedies"][0]["apply"]["fixed_sql"], fixed);
        assert_eq!(v["remedies"][0]["example"], fixed);
    }
}
