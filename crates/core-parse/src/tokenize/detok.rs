//! The inverse of tokenization: put real names and literal values back into an analysis envelope
//! and remap payload spans to original-text spans. Operates on the client-only
//! [`TokenMap`](super::TokenMap).

use super::{byte_to_loc, line_starts, loc_to_byte, TokenMap, TokenizeError};

/// Put real names/values back into an analysis envelope and remap payload spans to original-text
/// spans. `envelope` is the server's JSON; returns the detokenized JSON.
pub fn detokenize(envelope: &str, map: &TokenMap) -> Result<String, TokenizeError> {
    let mut v: serde_json::Value =
        serde_json::from_str(envelope).map_err(|e| TokenizeError::Internal(e.to_string()))?;
    detok_value(&mut v, map);
    serde_json::to_string_pretty(&v).map_err(|e| TokenizeError::Internal(e.to_string()))
}

fn detok_value(v: &mut serde_json::Value, map: &TokenMap) {
    match v {
        serde_json::Value::String(s) => *s = detok_text(s, map),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| detok_value(x, map)),
        serde_json::Value::Object(o) => {
            for (k, val) in o.iter_mut() {
                // a finding's `span` and an edit's `range` are both payload-coordinate spans;
                // a parameter's `spans` is an array of them.
                if k == "span" || k == "range" {
                    remap_span(val, map);
                } else if k == "spans" {
                    if let Some(arr) = val.as_array_mut() {
                        arr.iter_mut().for_each(|s| remap_span(s, map));
                    }
                } else {
                    detok_value(val, map);
                }
            }
        }
        _ => {}
    }
}

/// Replace every placeholder (identifier token `vqtN` and literal sentinel) in `text` with its
/// original source. Boundary-aware, so `vqt1` is never matched inside `vqt12` and a sentinel
/// number never inside a longer one — which also makes the order of replacement irrelevant.
fn detok_text(text: &str, map: &TokenMap) -> String {
    let mut out = text.to_string();
    for (token, source) in map.replacements() {
        out = replace_token(&out, token, source);
    }
    out
}

fn replace_token(text: &str, token: &str, source: &str) -> String {
    if token.is_empty() || !text.contains(token) {
        return text.to_string();
    }
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        if text[i..].starts_with(token) {
            let after = i + token.len();
            let before_ok = i == 0 || !is_word(b[i - 1]);
            let after_ok = after >= b.len() || !is_word(b[after]);
            if before_ok && after_ok {
                out.push_str(source);
                i = after;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Remap a `{start:{line,column}, end:{…}}` from payload positions to original ones:
/// payload (line,col) → payload byte → original byte (via segments) → original (line,col).
fn remap_span(span: &mut serde_json::Value, map: &TokenMap) {
    if !map.has_spans() {
        return;
    }
    let pay_starts = line_starts(map.payload());
    let orig_starts = line_starts(map.original());
    for end in ["start", "end"] {
        let Some(loc) = span.get_mut(end) else {
            continue;
        };
        let (Some(line), Some(col)) = (
            loc.get("line").and_then(|v| v.as_u64()),
            loc.get("column").and_then(|v| v.as_u64()),
        ) else {
            continue;
        };
        let pb = loc_to_byte(map.payload(), &pay_starts, line, col);
        let ob = map.payload_byte_to_orig(pb);
        let (l, c) = byte_to_loc(map.original(), &orig_starts, ob);
        loc["line"] = l.into();
        loc["column"] = c.into();
    }
}
