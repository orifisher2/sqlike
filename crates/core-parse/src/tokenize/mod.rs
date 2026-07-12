//! Privacy tokenizer (v0.3): rewrite a SQL string (and optional schema) into an opaque but
//! still-parseable, still-analyzable form — identifiers become stable tokens, literals become
//! value-free typed/shaped sentinels — plus a **client-only** map to put the real names back.
//!
//! Lexical, not semantic: it works on `sqlparser`'s token stream (positions + identifier /
//! keyword / literal classification), so it needs no analysis engine and preserves the original
//! text verbatim except at the spans it replaces.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::dialect::Dialect;
use crate::model::name::Name;

mod detok;
mod finalize;
pub use detok::detokenize;
pub use finalize::finalize;

/// Opaque identifier token prefix — a valid SQL identifier, distinctive enough that it won't
/// false-match ordinary words when detokenizing rule messages.
const PREFIX: &str = "vqt";

/// Built-in function names kept verbatim (the analysis reads them). Reserved-word functions
/// (`COUNT`, `CAST`, …) are already kept as keywords; this list is the *non-keyword* built-ins.
const BUILTINS: &[&str] = &[
    "lower",
    "upper",
    "trim",
    "length",
    "char_length",
    "octet_length",
    "substr",
    "substring",
    "coalesce",
    "nullif",
    "ifnull",
    "nvl",
    "greatest",
    "least",
    "abs",
    "round",
    "ceil",
    "ceiling",
    "floor",
    "mod",
    "power",
    "sqrt",
    "concat",
    "concat_ws",
    "replace",
    "split_part",
    "position",
    "strpos",
    "now",
    "date_trunc",
    "date_part",
    "to_char",
    "to_date",
    "to_timestamp",
    "extract",
    "string_agg",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "regexp_replace",
    "regexp_matches",
    "unnest",
    "generate_series",
    "random",
    "rand",
    // Reserved-word aggregates. sqlparser lexes these as keywords, but in function-call position
    // they still surface as identifiers the collector would tokenize — and the analysis reads the
    // name to recognize an aggregate, so they must survive verbatim. (Not user data.)
    "count",
    "sum",
    "avg",
    "min",
    "max",
];

#[derive(Debug)]
pub enum TokenizeError {
    /// The input didn't parse — the client validates before tokenizing, so this is a caller bug.
    Parse(String),
    Internal(String),
}

impl std::fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenizeError::Parse(m) => write!(f, "tokenize: cannot parse input: {m}"),
            TokenizeError::Internal(m) => write!(f, "tokenize: {m}"),
        }
    }
}
impl std::error::Error for TokenizeError {}

/// The result of [`tokenize`]: the opaque payload(s) for the wire, and the client-only map.
pub struct Tokenized {
    pub payload: String,
    pub schema_payload: Option<String>,
    /// Tokenized table-statistics JSON (`{token: row_count}`), keyed by the same identifier tokens
    /// as the payload so the server matches stats to tokenized tables. `None` when no stats given.
    pub stats_payload: Option<String>,
    /// Tokenized structured `Plan` JSON, its identifiers replaced by the payload's tokens (and
    /// literals dropped), so the server sharpens findings without seeing real names. `None` when no
    /// plan was given or it didn't parse.
    pub explain_payload: Option<String>,
    pub map: TokenMap,
}

/// The privacy boundary — never serialized to the wire. Maps every placeholder (identifier token
/// **and** literal sentinel) back to its original source text, and remaps payload spans to
/// original-text spans. Holds the query's original and payload text (both client-side already) so
/// 1-based line/col spans convert through byte offsets.
#[derive(Default)]
pub struct TokenMap {
    replacements: HashMap<String, String>,
    original: String,
    payload: String,
    /// Contiguous segments of the **query** payload: (tok range ↔ original range), for span remap.
    segments: Vec<Segment>,
}

struct Segment {
    tok_start: usize,
    tok_end: usize,
    orig_start: usize,
    orig_end: usize,
}

impl Segment {
    fn unchanged(&self) -> bool {
        self.tok_end - self.tok_start == self.orig_end - self.orig_start
    }
}

/// Accessors used by the [`detok`] submodule — keep `TokenMap`'s fields private to this module.
impl TokenMap {
    fn replacements(&self) -> &HashMap<String, String> {
        &self.replacements
    }
    fn has_spans(&self) -> bool {
        !self.segments.is_empty()
    }
    fn original(&self) -> &str {
        &self.original
    }
    fn payload(&self) -> &str {
        &self.payload
    }

    /// Map a payload byte offset to the corresponding original byte offset through the segments.
    fn payload_byte_to_orig(&self, pb: usize) -> usize {
        for seg in &self.segments {
            if pb >= seg.tok_start && pb <= seg.tok_end {
                return if seg.unchanged() {
                    seg.orig_start + (pb - seg.tok_start)
                } else if pb == seg.tok_start {
                    seg.orig_start
                } else {
                    seg.orig_end
                };
            }
        }
        self.original.len()
    }
}

/// Literal sentinels start here so they never collide with the small integers rewrites introduce
/// (`SELECT 1`, `LIMIT 1`); detok only restores recorded sentinels, but a high base keeps an
/// introduced `1` from ever looking like one.
const LIT_BASE: usize = 8_000_000;

/// Per-request token assignment. Identifiers are **consistent** (same name → same token, so joins
/// and correlations still analyze) and shared by query + schema; literals are **per-occurrence
/// distinct** (so value-equality across the query is never revealed) and recorded so the real
/// value can be restored in a `fix`. Both go in one `replacements` map keyed by placeholder text.
#[derive(Default)]
struct Assigner {
    name_to_token: HashMap<String, String>,
    replacements: HashMap<String, String>,
    next_ident: usize,
    next_lit: usize,
    /// When `Some`, literals are **value-consistent**: the same source value gets the same
    /// sentinel (keyed by source text + shape). Used by [`tokenize_pair`] so a literal-bearing
    /// predicate compares equal across the two queries. `None` (the default) keeps literals
    /// per-occurrence distinct — the privacy default that hides value-equality within a query.
    lit_consistency: Option<HashMap<(String, String), String>>,
}

impl Assigner {
    /// An assigner that gives equal literal values equal sentinels (equivalence pair mode).
    fn with_consistent_literals() -> Self {
        Assigner {
            lit_consistency: Some(HashMap::new()),
            ..Default::default()
        }
    }

    fn token_for(&mut self, key: String, original: &str) -> String {
        if let Some(t) = self.name_to_token.get(&key) {
            return t.clone();
        }
        let t = format!("{PREFIX}{}", self.next_ident);
        self.next_ident += 1;
        self.name_to_token.insert(key, t.clone());
        self.replacements.insert(t.clone(), original.to_string());
        t
    }

    fn literal_for(&mut self, source: &str, shape: LitShape) -> String {
        let key = (source.to_string(), shape.tag());
        if let Some(seen) = &self.lit_consistency {
            if let Some(s) = seen.get(&key) {
                return s.clone();
            }
        }
        let sentinel = shape.render(self.next_lit);
        self.next_lit += 1;
        self.replacements
            .insert(sentinel.clone(), source.to_string());
        if let Some(seen) = &mut self.lit_consistency {
            seen.insert(key, sentinel.clone());
        }
        sentinel
    }
}

/// What kind of value-free sentinel a literal renders to — preserving only the analysis-relevant
/// type (int vs decimal), LIKE-wildcard shape, and date shape, never the value. Strings whose
/// every character is a wildcard/escape (`'%'`, `''`) carry no private content and are kept
/// verbatim (see [`literal_raw`]), so they never reach this enum.
enum LitShape {
    Int,
    Decimal,
    /// A `YYYY-MM-DD` date-shaped string. Rendered date-shaped so `timestamp-compared-to-date`
    /// and `between-timestamp-date-bounds` still fire (reveals only that the value is a date).
    DateStr,
    /// A string with private content. `has_wild` records whether it holds any `%`/`_`/`\` so
    /// `like-without-wildcard` doesn't false-fire when the wildcard is interior.
    Str {
        lead: bool,
        trail: bool,
        has_wild: bool,
    },
}

impl LitShape {
    /// A stable discriminant for value-consistent literal keying — same source text under the
    /// same shape (int/decimal/like-wildcard) is the same literal.
    fn tag(&self) -> String {
        match self {
            LitShape::Int => "i".into(),
            LitShape::Decimal => "d".into(),
            LitShape::DateStr => "dt".into(),
            LitShape::Str {
                lead,
                trail,
                has_wild,
            } => format!("s{}{}{}", *lead as u8, *trail as u8, *has_wild as u8),
        }
    }

    fn render(&self, n: usize) -> String {
        match self {
            LitShape::Int => (LIT_BASE + n).to_string(),
            LitShape::Decimal => format!("{}.0", LIT_BASE + n),
            LitShape::DateStr => {
                let d = format!("{:08}", n % 100_000_000);
                format!("'{}-{}-{}'", &d[0..4], &d[4..6], &d[6..8])
            }
            LitShape::Str {
                lead,
                trail,
                has_wild,
            } => {
                let l = if *lead { "%" } else { "" };
                let t = if *trail { "%" } else { "" };
                // A wildcard that isn't at either end still has to show up, or a pattern like
                // `'a%b'` would look wildcard-free after tokenization.
                let mid = if *has_wild && !lead && !trail {
                    "%"
                } else {
                    ""
                };
                format!("'{l}vqs{n}{mid}{t}'")
            }
        }
    }
}

/// A `YYYY-MM-DD`-shaped string (digits with `-` at positions 4 and 7). Mirrors the check in the
/// `timestamp-compared-to-date` / `between-timestamp-date-bounds` rules.
fn is_date_shaped(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes().iter().enumerate().all(|(i, &b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Tokenize `sql` (and optional `schema`) under `dialect`. The query and schema share one token
/// assignment, so `users.email` in the query and `CREATE TABLE users(email …)` line up.
pub fn tokenize(
    sql: &str,
    schema: Option<&str>,
    dialect: Dialect,
) -> Result<Tokenized, TokenizeError> {
    tokenize_with_stats(sql, schema, None, None, dialect)
}

/// Like [`tokenize`] but also tokenizing a table-statistics JSON map `{"table": row_count}` and a
/// Postgres `EXPLAIN (FORMAT JSON)` plan — both keyed by the same identifier tokens as the payload,
/// so the server matches stats to tables and sharpens findings without ever seeing real names.
pub fn tokenize_with_stats(
    sql: &str,
    schema: Option<&str>,
    stats: Option<&str>,
    explain: Option<&str>,
    dialect: Dialect,
) -> Result<Tokenized, TokenizeError> {
    let mut a = Assigner::default();
    let (payload, segments) = rewrite(sql, dialect, &mut a)?;
    let schema_payload = match schema {
        Some(s) => Some(rewrite(s, dialect, &mut a)?.0),
        None => None,
    };
    let stats_payload = match stats {
        Some(s) => Some(tokenize_stats_keys(s, &a)?),
        None => None,
    };
    let explain_payload = explain.and_then(|e| tokenize_plan(e, &a, dialect));
    Ok(Tokenized {
        map: TokenMap {
            replacements: a.replacements,
            original: sql.to_string(),
            payload: payload.clone(),
            segments,
        },
        payload,
        schema_payload,
        stats_payload,
        explain_payload,
    })
}

/// The result of [`tokenize_pair`]: both query payloads and the client-only map. Used by the
/// equivalence path — two queries tokenized under **one** assigner, so a name shared by both
/// (e.g. `users`) gets the **same** token in each payload (else structural comparison would see
/// two different tables). The map is `replacements`-only (no span segments): a pair has two
/// source texts, so span remap is deferred to when the comparator emits span-bearing detail.
pub struct TokenizedPair {
    pub payload_a: String,
    pub payload_b: String,
    pub schema_payload: Option<String>,
    pub map: TokenMap,
}

/// Tokenize two queries (and an optional shared schema) under one shared assignment, for
/// equivalence comparison. Both resolve against the same schema — comparing queries over
/// *different* schemas is ill-posed (see `docs/phase-e2-two-query-contract.md`).
///
/// Literals are **value-consistent across the pair** (unlike the per-occurrence-distinct default),
/// so a literal-bearing predicate compares equal in both queries — otherwise Tier-1 *and* Tier-2
/// would fail on the common literal-bearing case. Privacy tradeoff, scoped to the equivalence
/// path: the server learns which literal positions *share a value* (within and across the pair),
/// never the values, and both queries are the caller's own (`docs/phase-e3-*` §literal finding).
pub fn tokenize_pair(
    sql_a: &str,
    sql_b: &str,
    schema: Option<&str>,
    dialect: Dialect,
) -> Result<TokenizedPair, TokenizeError> {
    let mut a = Assigner::with_consistent_literals();
    let (payload_a, _) = rewrite(sql_a, dialect, &mut a)?;
    let (payload_b, _) = rewrite(sql_b, dialect, &mut a)?;
    let schema_payload = match schema {
        Some(s) => Some(rewrite(s, dialect, &mut a)?.0),
        None => None,
    };
    Ok(TokenizedPair {
        payload_a,
        payload_b,
        schema_payload,
        map: TokenMap {
            replacements: a.replacements,
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod pair_tests {
    use super::*;

    #[test]
    fn shares_tokens_across_queries() {
        // `users` in both queries → the same token in both payloads (shared assigner).
        let p = tokenize_pair(
            "SELECT id FROM users",
            "SELECT name FROM users",
            None,
            Dialect::Postgres,
        )
        .unwrap();
        assert!(!p.payload_a.contains("users") && !p.payload_b.contains("users"));
        assert_eq!(
            p.payload_a.rsplit(' ').next(),
            p.payload_b.rsplit(' ').next(),
            "shared table token must match: a=`{}` b=`{}`",
            p.payload_a,
            p.payload_b
        );
    }

    #[test]
    fn identical_literal_free_queries_are_byte_identical() {
        let p = tokenize_pair(
            "SELECT a FROM t",
            "SELECT a FROM t",
            None,
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(p.payload_a, p.payload_b);
    }

    #[test]
    fn equal_literals_are_consistent_across_the_pair() {
        // Value-consistent literals (equivalence mode): the same source value gets the same
        // sentinel in both payloads, so a literal-bearing predicate compares equal.
        let p = tokenize_pair(
            "SELECT a FROM t WHERE x = 5",
            "SELECT a FROM t WHERE x = 5",
            None,
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(p.payload_a, p.payload_b);
        assert!(
            !p.payload_a.contains("= 5"),
            "literal not tokenized: {}",
            p.payload_a
        );
    }

    #[test]
    fn different_literal_values_get_different_sentinels() {
        let p = tokenize_pair(
            "SELECT a FROM t WHERE x = 5",
            "SELECT a FROM t WHERE x = 6",
            None,
            Dialect::Postgres,
        )
        .unwrap();
        assert_ne!(p.payload_a, p.payload_b);
    }
}

/// Tokenize a stats map's table-name keys with the same tokens as the payload, dropping tables the
/// query/schema never mentioned (they have no token and are irrelevant to its advice).
fn tokenize_stats_keys(stats_json: &str, a: &Assigner) -> Result<String, TokenizeError> {
    let raw: HashMap<String, u64> =
        serde_json::from_str(stats_json).map_err(|e| TokenizeError::Internal(e.to_string()))?;
    let tokenized: HashMap<&str, u64> = raw
        .iter()
        .filter_map(|(name, &count)| {
            // Match unquoted table names (Postgres folding); the key mirrors `ident_raw`.
            let key = format!("u:{}", name.to_ascii_lowercase());
            a.name_to_token.get(&key).map(|t| (t.as_str(), count))
        })
        .collect();
    serde_json::to_string(&tokenized).map_err(|e| TokenizeError::Internal(e.to_string()))
}

/// Tokenize a Postgres `EXPLAIN (FORMAT JSON)` plan: parse it, replace every identifier with the
/// query's token, and serialize the structured [`Plan`]. Literals in the plan's condition strings
/// are dropped at parse time (only column *names* survive), and the index name is dropped (the
/// verdict uses the served columns, not the name) — so nothing but query-derived tokens travels.
/// `None` if the plan doesn't parse (the caller simply sends no plan).
fn tokenize_plan(explain_json: &str, a: &Assigner, dialect: Dialect) -> Option<String> {
    let mut plan = crate::plan::Plan::from_explain(explain_json, dialect).ok()?;
    tokenize_plan_node(&mut plan.root, a);
    serde_json::to_string(&plan).ok()
}

fn tokenize_plan_node(node: &mut crate::plan::PlanNode, a: &Assigner) {
    let tok = |n: &Name| token_of(a, n);
    node.relation = node.relation.as_ref().and_then(tok);
    node.alias = node.alias.as_ref().and_then(tok);
    node.index_keys = node.index_keys.iter().filter_map(tok).collect();
    node.filtered = node.filtered.iter().filter_map(tok).collect();
    if let crate::plan::Access::IndexScan { index } = &mut node.access {
        *index = None; // the index name is a real identifier the verdict doesn't need
    }
    for child in &mut node.children {
        tokenize_plan_node(child, a);
    }
}

/// The token an identifier was assigned during tokenization, or `None` if the query never used it
/// (a plan referencing something outside the query — drop it rather than leak a real name). The key
/// mirrors `ident_raw`: unquoted names fold to lowercase, quoted names keep their case.
fn token_of(a: &Assigner, n: &Name) -> Option<Name> {
    let key = if n.quoted {
        format!("q:{}", n.text)
    } else {
        format!("u:{}", n.text.to_ascii_lowercase())
    };
    a.name_to_token
        .get(&key)
        .map(|t| Name::new(t.clone(), false))
}

/// One token's planned edit: replace the original byte range with `replacement`.
struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

/// A collected occurrence before its token is assigned (assignment is deferred to source order).
struct Raw {
    start: usize,
    end: usize,
    kind: Kind,
}

enum Kind {
    Ident { key: String, value: String },
    Literal { source: String, shape: LitShape },
}

fn rewrite(
    sql: &str,
    dialect: Dialect,
    a: &mut Assigner,
) -> Result<(String, Vec<Segment>), TokenizeError> {
    let stmts =
        crate::parser::parse(sql, dialect).map_err(|e| TokenizeError::Parse(e.to_string()))?;
    let json = serde_json::to_value(&stmts).map_err(|e| TokenizeError::Internal(e.to_string()))?;

    let starts = line_starts(sql);
    let mut raw: Vec<Raw> = Vec::new();
    collect(&json, sql, &starts, &mut raw, false);
    raw.sort_by_key(|r| r.start);
    raw.dedup_by_key(|r| r.start);
    // Assign tokens in source order (left to right) so numbering is stable and readable; the
    // shared `Assigner` keeps the query's idents numbered before the schema's.
    let edits: Vec<Edit> = raw
        .into_iter()
        .map(|r| Edit {
            start: r.start,
            end: r.end,
            replacement: match r.kind {
                Kind::Ident { key, value } => a.token_for(key, &value),
                Kind::Literal { source, shape } => a.literal_for(&source, shape),
            },
        })
        .collect();

    // Build the payload + the contiguous tok↔orig segment map.
    let mut payload = String::with_capacity(sql.len());
    let mut segments = Vec::new();
    let mut cursor = 0usize;
    for e in &edits {
        if e.start < cursor {
            continue; // overlapping (shouldn't happen) — skip defensively
        }
        if e.start > cursor {
            let tok_start = payload.len();
            payload.push_str(&sql[cursor..e.start]);
            segments.push(Segment {
                tok_start,
                tok_end: payload.len(),
                orig_start: cursor,
                orig_end: e.start,
            });
        }
        let tok_start = payload.len();
        payload.push_str(&e.replacement);
        segments.push(Segment {
            tok_start,
            tok_end: payload.len(),
            orig_start: e.start,
            orig_end: e.end,
        });
        cursor = e.end;
    }
    if cursor < sql.len() {
        let tok_start = payload.len();
        payload.push_str(&sql[cursor..]);
        segments.push(Segment {
            tok_start,
            tok_end: payload.len(),
            orig_start: cursor,
            orig_end: sql.len(),
        });
    }
    Ok((payload, segments))
}

/// Walk the serialized AST, collecting one edit per `Ident` (→ token) and per literal `Value`
/// (→ sentinel). Idents/literals are leaf nodes, so a generic walk catches them in every
/// position — select list, WHERE, aliases, CTE names, DDL columns — without enumerating AST types.
///
/// `keep_nums` marks a **control** context (`LIMIT`/`OFFSET`/`FETCH`/`TOP`): numbers there are
/// query *shape*, not user data, and rules read their magnitude (e.g. `offset-pagination`'s
/// 1000-row bound), so they're left verbatim — keeping them both analysis-faithful and private.
fn collect(v: &Value, sql: &str, starts: &[usize], raw: &mut Vec<Raw>, keep_nums: bool) {
    match v {
        Value::Object(o) => {
            if let Some(r) =
                ident_raw(o, sql, starts).or_else(|| literal_raw(o, sql, starts, keep_nums))
            {
                raw.push(r);
                return;
            }
            for (k, val) in o {
                let control = matches!(
                    k.as_str(),
                    "limit" | "offset" | "fetch" | "limit_by" | "top"
                );
                collect(val, sql, starts, raw, keep_nums || control);
            }
        }
        Value::Array(arr) => arr
            .iter()
            .for_each(|x| collect(x, sql, starts, raw, keep_nums)),
        _ => {}
    }
}

/// An `Ident` serializes as `{ value: String, quote_style: <char|null>, span }` — its fingerprint.
fn ident_raw(o: &Map<String, Value>, sql: &str, starts: &[usize]) -> Option<Raw> {
    let value = o.get("value")?.as_str()?;
    if o.len() > 3 || !o.contains_key("span") {
        return None;
    }
    let quoted = o.get("quote_style").is_some_and(Value::is_string);
    if !quoted && BUILTINS.contains(&value.to_ascii_lowercase().as_str()) {
        return None; // a built-in function name — the analysis reads it, keep
    }
    // Reserved value-keywords carry no private data — keep them verbatim so analysis can read
    // them (equals-null, `CASE … ELSE NULL`, …). `Value::Null` in particular serializes its value
    // as the bare string `"Null"`, which otherwise looks exactly like an unquoted identifier.
    if !quoted
        && matches!(
            value.to_ascii_lowercase().as_str(),
            "null" | "true" | "false" | "default"
        )
    {
        return None;
    }
    let key = if quoted {
        format!("q:{value}")
    } else {
        format!("u:{}", value.to_ascii_lowercase())
    };
    let (start, end) = span_bytes(o.get("span")?, sql, starts)?;
    Some(Raw {
        start,
        end,
        kind: Kind::Ident {
            key,
            value: value.to_string(),
        },
    })
}

/// A literal serializes as `ValueWithSpan { value: Value, span }` — `value` is an object whose
/// single key is the variant (`Number`, `SingleQuotedString`, …). Booleans/NULL/placeholders
/// carry no private value and are kept.
fn literal_raw(
    o: &Map<String, Value>,
    sql: &str,
    starts: &[usize],
    keep_nums: bool,
) -> Option<Raw> {
    if o.len() != 2 {
        return None;
    }
    let val = o.get("value")?.as_object()?;
    let (kind, inner) = val.iter().next()?;
    if keep_nums && kind == "Number" {
        return None; // a control bound (LIMIT/OFFSET/…) — shape, not data; keep verbatim
    }
    let (start, end) = span_bytes(o.get("span")?, sql, starts)?;
    let source = sql.get(start..end)?;
    // `0`/`1` are low-entropy structural constants (existence tests like `count(*) > 0`, the
    // `THEN 1 ELSE 0` count idiom, boolean flags) — shape, not data. Keeping them verbatim lets
    // value-dependent rules survive tokenization, and reveals essentially nothing private.
    if kind == "Number" && matches!(source, "0" | "1") {
        return None;
    }
    let shape = match kind.as_str() {
        "Number" if source.contains('.') || source.contains(['e', 'E']) => LitShape::Decimal,
        "Number" => LitShape::Int,
        k if k.contains("String") || k.contains("Literal") => {
            let v = inner.as_str().unwrap_or("");
            if is_date_shaped(v) {
                LitShape::DateStr
            } else if v.chars().all(|c| matches!(c, '%' | '_' | '\\')) {
                // Only wildcards/escapes (or empty) — no private content, so keep it verbatim
                // (this is what lets `like-all-wildcards` still see `'%'`).
                return None;
            } else {
                LitShape::Str {
                    lead: v.starts_with(['%', '_']),
                    trail: v.ends_with(['%', '_']),
                    has_wild: v.contains(['%', '_', '\\']),
                }
            }
        }
        _ => return None,
    };
    Some(Raw {
        start,
        end,
        kind: Kind::Literal {
            source: source.to_string(),
            shape,
        },
    })
}

/// Read a serialized `Span` (`{start:{line,column}, end:{…}}`) as a byte range in `sql`.
fn span_bytes(span: &Value, sql: &str, starts: &[usize]) -> Option<(usize, usize)> {
    let loc = |k: &str| -> Option<usize> {
        let p = span.get(k)?;
        Some(loc_to_byte(
            sql,
            starts,
            p.get("line")?.as_u64()?,
            p.get("column")?.as_u64()?,
        ))
    };
    let (start, end) = (loc("start")?, loc("end")?);
    (start < end && end <= sql.len()).then_some((start, end))
}

fn line_starts(s: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// 1-based (line, col) → byte offset in `s` (col counts chars, UTF-8 safe).
fn loc_to_byte(s: &str, starts: &[usize], line: u64, column: u64) -> usize {
    let line = (line.max(1) as usize - 1).min(starts.len() - 1);
    let base = starts[line];
    let col = column.max(1) as usize - 1;
    s[base..]
        .char_indices()
        .nth(col)
        .map(|(i, _)| base + i)
        .unwrap_or(s.len())
}

/// byte offset → 1-based (line, col) in `s`.
fn byte_to_loc(s: &str, starts: &[usize], byte: usize) -> (u64, u64) {
    let byte = byte.min(s.len());
    let line = starts.partition_point(|&p| p <= byte) - 1;
    let col = s[starts[line]..byte].chars().count() + 1;
    (line as u64 + 1, col as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> Tokenized {
        tokenize(sql, None, Dialect::Postgres).unwrap()
    }

    #[test]
    fn plan_tokenizes_identifiers_and_drops_literals() {
        use crate::plan::{Plan, Verdict};
        let mut a = Assigner::default();
        rewrite(
            "SELECT * FROM users WHERE status = 'active'",
            Dialect::Postgres,
            &mut a,
        )
        .unwrap();

        let explain = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"users",
            "Filter":"(status = 'active'::text)","Actual Rows":5000000}}]"#;
        let payload = tokenize_plan(explain, &a, Dialect::Postgres).unwrap();

        for leaked in ["users", "status", "active"] {
            assert!(!payload.contains(leaked), "leaked {leaked}: {payload}");
        }

        // The tokenized plan deserializes and its verdict matches on the query's tokens.
        let plan: Plan = serde_json::from_str(&payload).unwrap();
        let table = a.name_to_token.get("u:users").unwrap();
        let column = a.name_to_token.get("u:status").unwrap();
        assert_eq!(
            plan.verdict(table, None, column),
            Verdict::Confirm {
                actual_rows: Some(5000000)
            }
        );
    }

    #[test]
    fn null_keyword_is_kept_verbatim() {
        // `Value::Null` serializes as the bare string "Null" — it must be kept, not tokenized as
        // an identifier, else `= NULL` / `ELSE NULL` go opaque and NULL-aware analysis dies on the
        // wire (the shipped `equals-null` rule, and equivalence normalization).
        for sql in [
            "SELECT id FROM t WHERE note = NULL",
            "SELECT COALESCE(a, NULL) FROM t",
            "SELECT CASE WHEN a > 0 THEN 1 ELSE NULL END FROM t",
        ] {
            let t = pg(sql);
            assert!(t.payload.contains("NULL"), "NULL dropped: {}", t.payload);
        }
    }

    #[test]
    fn identifiers_tokenize_keywords_and_builtins_stay() {
        let t = pg("SELECT lower(email) FROM users WHERE id = 42");
        assert!(t.payload.starts_with("SELECT lower("), "{}", t.payload);
        for kw in ["SELECT", "FROM", "WHERE", "lower"] {
            assert!(t.payload.contains(kw), "{kw} dropped: {}", t.payload);
        }
        for leaked in ["email", "users", "id", "42"] {
            assert!(
                !t.payload.contains(leaked),
                "leaked {leaked}: {}",
                t.payload
            );
        }
        assert!(t.payload.contains("vqt"), "no tokens: {}", t.payload);
        // 42 → a high-base integer sentinel (never the small ints rewrites introduce)
        assert!(
            t.payload.contains(&LIT_BASE.to_string()),
            "no number sentinel: {}",
            t.payload
        );
    }

    #[test]
    fn same_identifier_gets_one_consistent_token() {
        let t = pg("SELECT a, a, b");
        // a → vqt0 (twice), b → vqt1
        assert_eq!(t.payload, "SELECT vqt0, vqt0, vqt1", "{}", t.payload);
    }

    #[test]
    fn query_and_schema_share_tokens() {
        let t = tokenize(
            "SELECT email FROM users",
            Some("CREATE TABLE users (email text)"),
            Dialect::Postgres,
        )
        .unwrap();
        let schema = t.schema_payload.unwrap();
        // whatever token `users`/`email` got in the query, the schema reuses it
        assert!(
            !schema.contains("users") && !schema.contains("email"),
            "{schema}"
        );
        assert!(schema.contains("vqt"), "{schema}");
        // users → vqt1 in the query ("SELECT vqt0 FROM vqt1"); schema's table is the same
        assert_eq!(t.payload, "SELECT vqt0 FROM vqt1", "{}", t.payload);
        assert!(
            schema.contains("vqt1"),
            "schema lost the shared table token: {schema}"
        );
    }

    #[test]
    fn quoted_identifiers_tokenize_and_case_folds() {
        // unquoted Email and email fold together; "Email" quoted is distinct
        let t = pg(r#"SELECT Email, email, "Email" FROM t"#);
        assert_eq!(
            t.payload, "SELECT vqt0, vqt0, vqt1 FROM vqt2",
            "{}",
            t.payload
        );
    }

    #[test]
    fn string_sentinels_preserve_only_wildcard_shape() {
        // value-free (no `foo`), distinctive (`vqs`), wildcard shape preserved
        let p = pg("SELECT * FROM t WHERE c LIKE '%foo'").payload;
        assert!(p.contains("'%vqs") && !p.contains("foo"), "{p}");
        assert!(pg("SELECT * FROM t WHERE c LIKE 'foo%'")
            .payload
            .contains("vqs0%'"));
        assert!(pg("SELECT * FROM t WHERE c LIKE '%foo%'")
            .payload
            .contains("'%vqs0%'"));
        let plain = pg("SELECT * FROM t WHERE c = 'foo'").payload;
        assert!(plain.contains("'vqs") && !plain.contains('%'), "{plain}");
        assert!(pg("SELECT x FROM t WHERE y = 1.5").payload.contains(".0"));
    }

    #[test]
    fn literals_are_distinct_per_occurrence_and_restore() {
        // value-equality is hidden: two `5`s get *different* sentinels...
        let t = pg("SELECT * FROM t WHERE a = 5 OR a = 5");
        let sentinels: Vec<_> = t
            .payload
            .split_whitespace()
            .filter(|w| w.parse::<u64>().is_ok())
            .collect();
        assert_eq!(sentinels.len(), 2);
        assert_ne!(
            sentinels[0], sentinels[1],
            "value-equality leaked: {}",
            t.payload
        );
        assert!(!t.payload.contains(" 5"), "value leaked: {}", t.payload);

        // ...but detok restores the real values (a fix the server built over tokens)
        let fix = format!(r#"{{"fix":"a IN ({}, {})"}}"#, sentinels[0], sentinels[1]);
        let out = detokenize(&fix, &t.map).unwrap();
        assert!(out.contains("a IN (5, 5)"), "{out}");
    }

    #[test]
    fn detok_restores_values_in_fix_and_edits() {
        let t = pg("SELECT * FROM o WHERE x = 100 OR x = 250");
        let s: Vec<String> = t
            .map
            .replacements()
            .keys()
            .filter(|k| k.parse::<u64>().is_ok())
            .cloned()
            .collect();
        assert_eq!(s.len(), 2, "two distinct integer sentinels");

        // a server fix + edit over tokens; detok restores both real values (order-independent)
        let env = format!(
            r#"{{"fix":"x IN ({}, {})","edits":[{{"range":{{"start":{{"line":1,"column":1}},"end":{{"line":1,"column":2}}}},"replacement":"IN ({}, {})"}}]}}"#,
            s[0], s[1], s[0], s[1]
        );
        let out = detokenize(&env, &t.map).unwrap();
        for v in ["100", "250"] {
            assert!(out.contains(v), "value {v} not restored: {out}");
        }
    }

    #[test]
    fn privacy_gate_no_identifier_or_value_leaks() {
        // The hard gate: distinctive sensitive names/values must never reach the payload. Includes
        // identifiers that are sqlparser keywords (`name`, `value`, `token`) — the lexer leaked
        // those — and a leading-wildcard value whose shape we keep but whose text we don't.
        let cases = [
            "SELECT ssn, salary FROM employees WHERE name = 'topsecret'",
            "SELECT secret_col FROM private_table WHERE token = 'abc123xyz'",
            r#"SELECT "MixedCase" FROM "Schema"."Tbl" WHERE value = 42424242"#,
            "SELECT * FROM patients WHERE diagnosis LIKE '%cancer%'",
        ];
        let sensitive = [
            "ssn",
            "salary",
            "employees",
            "topsecret",
            "secret_col",
            "private_table",
            "abc123xyz",
            "MixedCase",
            "Schema",
            "Tbl",
            "patients",
            "diagnosis",
            "cancer",
            "42424242",
        ];
        for sql in cases {
            let t = pg(sql);
            for s in sensitive {
                assert!(
                    !t.payload.contains(s),
                    "leaked `{s}` in payload `{}`",
                    t.payload
                );
                assert!(
                    t.schema_payload.as_deref().is_none_or(|p| !p.contains(s)),
                    "leaked `{s}` in schema payload"
                );
            }
        }
    }

    #[test]
    fn detokenize_restores_names_in_text() {
        let t = pg("SELECT secret FROM t");
        let out = detokenize(r#"{"message":"column vqt0 is unknown"}"#, &t.map).unwrap();
        assert!(out.contains("column secret is unknown"), "{out}");
    }

    #[test]
    fn detokenize_remaps_span_to_original() {
        // "SELECT email FROM t" → "SELECT vqt0 FROM vqt1"; email is cols 8..13, vqt0 cols 8..12
        let t = pg("SELECT email FROM t");
        assert_eq!(t.payload, "SELECT vqt0 FROM vqt1");
        let env = r#"{"span":{"start":{"line":1,"column":8},"end":{"line":1,"column":12}}}"#;
        let out = detokenize(env, &t.map).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["span"]["start"]["column"], 8);
        assert_eq!(
            v["span"]["end"]["column"], 13,
            "email ends at col 13: {out}"
        );
    }
}
