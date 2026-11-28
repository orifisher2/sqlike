//! The algebra a `diff` pair gets before its literals are hidden (UR19 step 4, the owner's D4).
//!
//! The server reads stand-ins by identity only: two queries that mean the same comparison prove
//! equal there only if they carry the same literal. So, on the real values and before tokenizing,
//! each comparison of a column with literals is rewritten into one form:
//!
//! - a literal moves across the comparison: `seats + 1 > 6` and `seats * 2 > 10` become
//!   `seats > 5`, flipping the comparison for a negative factor, and only when the division is
//!   exact;
//! - arithmetic between integer literals is done: `seats > 2 + 3` becomes `seats > 5`;
//! - a number is written one way: `5.0` and `05` become `5`;
//! - on an integer column, strict bounds become inclusive (`seats > 4` to `seats >= 5`) and
//!   `BETWEEN` becomes its two bounds, so `seats > 4 AND seats < 11` meets `BETWEEN 5 AND 10`.
//!
//! Exact or not at all: integers only, no division between literals, and the integer rewrites only
//! where every table the statement reads agrees the column is an integer and no CTE or derived
//! table could stand in for one. Each rewrite edits the comparison's own text, so the rest of the
//! query reaches the tokenizer as the user wrote it. `check` does not use this: its findings point
//! at positions in the user's query.

use std::collections::HashSet;

use serde_json::Value;

use super::Lines;
use crate::dialect::Dialect;
use crate::model::ty::Type;
use crate::schema::Schema;

/// `sql` with its comparisons in canonical form, or `sql` unchanged when it does not parse as one
/// statement.
pub(super) fn canonical(sql: &str, schema: Option<&str>, dialect: Dialect) -> String {
    let Ok(stmts) = crate::parser::parse(sql, dialect) else {
        return sql.to_string();
    };
    let [stmt] = stmts.as_slice() else {
        return sql.to_string();
    };
    let Ok(json) = serde_json::to_value(stmt) else {
        return sql.to_string();
    };
    let ints = schema
        .and_then(|ddl| Schema::from_ddl_with(ddl, dialect).ok())
        .map(|s| integer_columns(&json, &s))
        .unwrap_or_default();
    let mut c = Canon {
        sql,
        lines: Lines::of(sql),
        ints,
        edits: Vec::new(),
    };
    c.walk(&json);
    let mut out = sql.to_string();
    c.edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (start, end, text) in c.edits {
        out.replace_range(start..end, &text);
    }
    out
}

/// The column names an integer rewrite may trust: integer in every schema table the statement reads
/// that has them, when no CTE or derived table could stand in for one, and not a select-list alias.
fn integer_columns(stmt: &Value, schema: &Schema) -> HashSet<String> {
    let mut read = HashSet::new();
    let mut aliases = HashSet::new();
    let mut opaque = false;
    scan(stmt, &mut read, &mut aliases, &mut opaque);
    if opaque {
        return HashSet::new();
    }
    let tables: Vec<_> = schema
        .tables()
        .into_iter()
        .filter(|(name, _)| read.contains(&name.to_lowercase()))
        .map(|(_, t)| t)
        .collect();
    let mut ints = HashSet::new();
    let mut other = HashSet::new();
    for t in tables {
        for c in &t.columns {
            let name = c.name.normalized();
            if c.ty == Type::Integer {
                ints.insert(name);
            } else {
                other.insert(name);
            }
        }
    }
    ints.retain(|c| !other.contains(c) && !aliases.contains(c));
    ints
}

fn scan(v: &Value, read: &mut HashSet<String>, aliases: &mut HashSet<String>, opaque: &mut bool) {
    match v {
        Value::Object(o) => {
            if o.contains_key("Derived") || o.get("with").is_some_and(|w| !w.is_null()) {
                *opaque = true;
            }
            if let Some(Value::Object(t)) = o.get("Table") {
                if let Some(last) = t
                    .get("name")
                    .and_then(Value::as_array)
                    .and_then(|n| n.last())
                {
                    if let Some(name) = last.pointer("/Identifier/value").and_then(Value::as_str) {
                        read.insert(name.to_lowercase());
                    }
                }
            }
            if let Some(alias) = o
                .get("ExprWithAlias")
                .and_then(|a| a.pointer("/alias/value"))
                .and_then(Value::as_str)
            {
                aliases.insert(alias.to_lowercase());
            }
            o.values().for_each(|x| scan(x, read, aliases, opaque));
        }
        Value::Array(a) => a.iter().for_each(|x| scan(x, read, aliases, opaque)),
        _ => {}
    }
}

struct Canon<'a> {
    sql: &'a str,
    lines: Lines,
    ints: HashSet<String>,
    edits: Vec<(usize, usize, String)>,
}

/// An integer literal's value, through `Nested` and integer `+ - *`.
fn eval(v: &Value) -> Option<i64> {
    if let Some(n) = v.pointer("/Value/value/Number/0").and_then(Value::as_str) {
        return integral(n);
    }
    if let Some(inner) = v.get("Nested") {
        return eval(inner);
    }
    if let Some(u) = v.get("UnaryOp") {
        return match u.get("op")?.as_str()? {
            "Minus" => eval(u.get("expr")?)?.checked_neg(),
            "Plus" => eval(u.get("expr")?),
            _ => None,
        };
    }
    let b = v.get("BinaryOp")?;
    let (l, r) = (eval(b.get("left")?)?, eval(b.get("right")?)?);
    match b.get("op")?.as_str()? {
        "Plus" => l.checked_add(r),
        "Minus" => l.checked_sub(r),
        "Multiply" => l.checked_mul(r),
        _ => None,
    }
}

/// `05`, `5.0` and `5.00` are the integer 5; `5.5` is not an integer.
fn integral(n: &str) -> Option<i64> {
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    if !frac.bytes().all(|b| b == b'0') || whole.is_empty() {
        return None;
    }
    whole.parse().ok()
}

fn flipped(op: &str) -> Option<&'static str> {
    Some(match op {
        "Gt" => "Lt",
        "GtEq" => "LtEq",
        "Lt" => "Gt",
        "LtEq" => "GtEq",
        "Eq" => "Eq",
        "NotEq" => "NotEq",
        _ => return None,
    })
}

fn symbol(op: &str) -> Option<&'static str> {
    Some(match op {
        "Gt" => ">",
        "GtEq" => ">=",
        "Lt" => "<",
        "LtEq" => "<=",
        "Eq" => "=",
        "NotEq" => "<>",
        _ => return None,
    })
}

/// A bare column: `seats` or `t.seats`, with its name.
fn column(v: &Value) -> Option<String> {
    let ident = match v {
        Value::Object(o) if o.contains_key("Identifier") => o.get("Identifier")?,
        Value::Object(o) => o.get("CompoundIdentifier")?.as_array()?.last()?,
        _ => return None,
    };
    let name = ident.get("value")?.as_str()?;
    Some(match ident.get("quote_style") {
        Some(Value::Null) | None => name.to_lowercase(),
        Some(_) => name.to_string(),
    })
}

impl Canon<'_> {
    fn walk(&mut self, v: &Value) {
        match v {
            Value::Object(o) => {
                if let Some(b) = o.get("BinaryOp") {
                    if self.comparison(b) || self.fold(v) {
                        return;
                    }
                }
                if let Some(b) = o.get("Between") {
                    if self.between(b) {
                        return;
                    }
                }
                if self.spelling(v) {
                    return;
                }
                o.values().for_each(|x| self.walk(x));
            }
            Value::Array(a) => a.iter().for_each(|x| self.walk(x)),
            _ => {}
        }
    }

    /// `column op literal`, after moving every literal across, in its canonical spelling.
    fn comparison(&mut self, b: &Value) -> bool {
        let Some(op) = b.get("op").and_then(Value::as_str) else {
            return false;
        };
        let (Some(left), Some(right)) = (b.get("left"), b.get("right")) else {
            return false;
        };
        let Some(_) = symbol(op) else {
            return false;
        };
        // The literal goes on the right.
        let (mut side, mut k, mut op) = match (eval(left), eval(right)) {
            (None, Some(k)) => (left, k, op),
            (Some(k), None) => (right, k, flipped(op).unwrap_or(op)),
            _ => return false,
        };
        // Move literals across until a column is left.
        for _ in 0..4 {
            if column(side).is_some() {
                break;
            }
            let inner = side.get("Nested").unwrap_or(side);
            let Some(ib) = inner.get("BinaryOp") else {
                return false;
            };
            let (Some(il), Some(ir), Some(iop)) = (
                ib.get("left"),
                ib.get("right"),
                ib.get("op").and_then(Value::as_str),
            ) else {
                return false;
            };
            let moved = match (iop, eval(il), eval(ir)) {
                ("Plus", None, Some(c)) => k.checked_sub(c).map(|k| (il, k, op)),
                ("Plus", Some(c), None) => k.checked_sub(c).map(|k| (ir, k, op)),
                ("Minus", None, Some(c)) => k.checked_add(c).map(|k| (il, k, op)),
                ("Minus", Some(c), None) => {
                    c.checked_sub(k).and_then(|k| Some((ir, k, flipped(op)?)))
                }
                ("Multiply", None, Some(c)) | ("Multiply", Some(c), None)
                    if c != 0 && k % c == 0 =>
                {
                    let x = if eval(il).is_none() { il } else { ir };
                    let op = if c < 0 { flipped(op) } else { Some(op) };
                    op.map(|op| (x, k / c, op))
                }
                _ => None,
            };
            let Some((x, nk, nop)) = moved else {
                return false;
            };
            (side, k, op) = (x, nk, nop);
        }
        let Some(name) = column(side) else {
            return false;
        };
        if self.ints.contains(&name) {
            (k, op) = match op {
                "Gt" => match k.checked_add(1) {
                    Some(k) => (k, "GtEq"),
                    None => return false,
                },
                "Lt" => match k.checked_sub(1) {
                    Some(k) => (k, "LtEq"),
                    None => return false,
                },
                other => (k, other),
            };
        }
        let Some(col) = self.text_of(side) else {
            return false;
        };
        let text = format!("{col} {} {k}", symbol(op).expect("a comparison"));
        self.replace(&Value::Array(vec![left.clone(), right.clone()]), text)
    }

    /// `x BETWEEN a AND b` on an integer column, as its two inclusive bounds.
    fn between(&mut self, b: &Value) -> bool {
        if b.get("negated").and_then(Value::as_bool) != Some(false) {
            return false;
        }
        let (Some(expr), Some(low), Some(high)) = (b.get("expr"), b.get("low"), b.get("high"))
        else {
            return false;
        };
        let (Some(name), Some(lo), Some(hi)) = (column(expr), eval(low), eval(high)) else {
            return false;
        };
        if !self.ints.contains(&name) {
            return false;
        }
        let Some(col) = self.text_of(expr) else {
            return false;
        };
        let text = format!("({col} >= {lo} AND {col} <= {hi})");
        self.replace(&Value::Array(vec![expr.clone(), high.clone()]), text)
    }

    /// Every integer literal written one way, wherever it stands (`05` is `5`), so two different
    /// integer stand-ins are two different values; the server may then rely on it (UR19,
    /// `canonical_numbers`).
    fn spelling(&mut self, v: &Value) -> bool {
        let Some(n) = v.pointer("/Value/value/Number/0").and_then(Value::as_str) else {
            return false;
        };
        if !n.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let canonical = n.trim_start_matches('0');
        let canonical = if canonical.is_empty() { "0" } else { canonical };
        canonical == n || self.replace(v, canonical.to_string())
    }

    /// Integer arithmetic between literals alone, done.
    fn fold(&mut self, v: &Value) -> bool {
        let Some(n) = eval(v) else {
            return false;
        };
        if v.pointer("/Value").is_some() {
            return false;
        }
        self.replace(v, n.to_string())
    }

    /// The source text the leaves of `v` cover.
    fn text_of(&self, v: &Value) -> Option<String> {
        let (s, e) = self.range(v)?;
        Some(self.sql[s..e].to_string())
    }

    /// The byte range the leaves of `v` cover, widened to keep parentheses balanced.
    fn range(&self, v: &Value) -> Option<(usize, usize)> {
        let mut spans = Vec::new();
        leaf_spans(v, &mut spans);
        let at = |p: &Value| -> Option<usize> {
            Some(self.lines.byte_at(
                self.sql,
                p.get("line")?.as_u64()?,
                p.get("column")?.as_u64()?,
            ))
        };
        let mut start = usize::MAX;
        let mut end = 0;
        for s in spans {
            start = start.min(at(s.get("start")?)?);
            end = end.max(at(s.get("end")?)?);
        }
        if start >= end {
            return None;
        }
        // A `Nested` operand's parentheses are not leaves: take in each one the range opens or
        // closes.
        loop {
            let t = &self.sql[start..end];
            let (open, close) = (t.matches('(').count(), t.matches(')').count());
            if close > open {
                start = self.sql[..start].rfind('(')?;
            } else if open > close {
                end += self.sql[end..].find(')')? + 1;
            } else {
                return Some((start, end));
            }
        }
    }

    fn replace(&mut self, v: &Value, text: String) -> bool {
        let Some((start, end)) = self.range(v) else {
            return false;
        };
        if self.sql[start..end] == text {
            return true;
        }
        self.edits.push((start, end, text));
        true
    }
}

fn leaf_spans<'v>(v: &'v Value, out: &mut Vec<&'v Value>) {
    match v {
        Value::Object(o) => {
            if let Some(span) = o.get("span") {
                out.push(span);
            }
            o.iter()
                .filter(|(k, _)| *k != "span")
                .for_each(|(_, x)| leaf_spans(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| leaf_spans(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = "CREATE TABLE s (id int PRIMARY KEY, seats int NOT NULL, price numeric)";

    fn canon(sql: &str) -> String {
        canonical(sql, Some(SCHEMA), Dialect::Postgres)
    }

    #[test]
    fn literals_move_across_and_fold() {
        assert_eq!(
            canon("SELECT id FROM s WHERE seats * 2 > 10"),
            "SELECT id FROM s WHERE seats >= 6"
        );
        assert_eq!(
            canon("SELECT id FROM s WHERE seats + 1 > 6"),
            "SELECT id FROM s WHERE seats >= 6"
        );
        assert_eq!(
            canon("SELECT id FROM s WHERE seats > 2 + 3"),
            "SELECT id FROM s WHERE seats >= 6"
        );
        assert_eq!(
            canon("SELECT id FROM s WHERE 10 < (seats * 2)"),
            "SELECT id FROM s WHERE seats >= 6"
        );
        assert_eq!(
            canon("SELECT id FROM s WHERE price * -2 >= 10"),
            "SELECT id FROM s WHERE price <= -5"
        );
    }

    #[test]
    fn integer_bounds_meet() {
        let a = canon("SELECT id FROM s WHERE seats > 4 AND seats < 11");
        let b = canon("SELECT id FROM s WHERE seats BETWEEN 5 AND 10");
        assert_eq!(a, "SELECT id FROM s WHERE seats >= 5 AND seats <= 10");
        assert_eq!(b, "SELECT id FROM s WHERE (seats >= 5 AND seats <= 10)");
        assert_eq!(
            canon("SELECT id FROM s WHERE seats > 5.0"),
            canon("SELECT id FROM s WHERE seats > 5")
        );
    }

    /// Not exact, not an integer, not known: left as written.
    #[test]
    fn what_is_left_alone() {
        for sql in [
            "SELECT id FROM s WHERE seats * 2 > 11",
            "SELECT id FROM s WHERE price > 4.5",
            "SELECT id FROM s WHERE seats / 2 > 5",
            "WITH c AS (SELECT seats FROM s) SELECT seats FROM c WHERE seats < 11",
            "SELECT id FROM s WHERE seats * 0 > 0",
        ] {
            assert_eq!(canon(sql), sql);
        }
        // An integer is written one way everywhere, not only in a comparison.
        assert_eq!(
            canon("SELECT * FROM (VALUES (05), (5), (007)) AS v (x)"),
            "SELECT * FROM (VALUES (5), (5), (7)) AS v (x)"
        );
        // A non-integer column keeps its strict bound.
        assert_eq!(
            canon("SELECT id FROM s WHERE price > 4"),
            "SELECT id FROM s WHERE price > 4"
        );
    }
}
