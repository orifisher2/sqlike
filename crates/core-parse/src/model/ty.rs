//! SQL types, coarse-grained to what v0.1 analysis needs.
//!
//! Finer distinctions (integer widths, timezone-awareness, array element types)
//! are added when a rule actually depends on them. `Unknown` is the honest default
//! before schema-aware resolution fills real types in (F4).

use crate::parser::ast;

/// A SQL value type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Integer,
    Numeric,
    Float,
    Text,
    /// Fixed-width `CHAR(n)`/`bpchar`. Distinct from `Text` because it space-pads and compares
    /// trailing spaces differently under `=` vs `LIKE` — which the `like-without-wildcard`
    /// rewrite must respect.
    Char,
    Boolean,
    Date,
    Time,
    Timestamp,
    Uuid,
    Json,
    /// A named or otherwise-unsupported type, preserved verbatim for diagnostics.
    Other(String),
    /// Not yet resolved: no schema available, or inference is incomplete.
    Unknown,
}

impl Type {
    /// Map a sqlparser `DataType` to a coarse VARQ `Type`. Shared by `translate`
    /// (for `CAST`) and the schema parser (for column types) so the mapping lives
    /// in one place. Matched on the rendered name to stay robust across the many
    /// `DataType` variants.
    pub fn from_ast(dt: &ast::DataType) -> Type {
        let s = dt.to_string().to_lowercase();
        // `signed`/`unsigned` are MySQL's integer cast targets (`CAST(x AS SIGNED)`); `INT
        // UNSIGNED` already matches `int`, so this only adds the bare cast forms.
        if s.contains("int") || s.contains("signed") {
            Type::Integer
        } else if s.contains("bool") {
            Type::Boolean
        } else if s.contains("numeric") || s.contains("decimal") {
            Type::Numeric
        } else if s.contains("real") || s.contains("double") || s.contains("float") {
            Type::Float
        } else if s.contains("timestamp") {
            Type::Timestamp
        } else if s.contains("date") {
            Type::Date
        } else if s.contains("time") {
            Type::Time
        } else if s.contains("uuid") {
            Type::Uuid
        } else if s.contains("json") {
            Type::Json
        } else if s.contains("char") || s.contains("text") {
            // Fixed-width CHAR(n)/bpchar/character(n) — but NOT varchar / character varying.
            let fixed = (s.starts_with("char") || s.starts_with("character") || s == "bpchar")
                && !s.contains("varying");
            if fixed {
                Type::Char
            } else {
                Type::Text
            }
        } else {
            Type::Other(s)
        }
    }
}
