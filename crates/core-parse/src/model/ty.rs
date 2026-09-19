//! SQL types, coarse-grained to what analysis needs.
//!
//! Finer distinctions (integer widths, timezone-awareness, array element types)
//! are added when a rule actually depends on them. `Unknown` is the honest default
//! before schema-aware resolution fills real types in (F4). The exact-numeric and
//! character widths are kept because a cast to a wider type of the same kind holds
//! every value of the column and reads as the column (`docs/phase-tw1-type-widths.md`);
//! two widths are two types, so the derived equality tells them apart.

use crate::parser::ast;
use crate::parser::ast::{CharacterLength, DataType, ExactNumberInfo};

/// A SQL value type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Integer,
    /// `NUMERIC(p, s)`/`DECIMAL(p, s)`; `None` for an unconstrained one. `DECIMAL(p)` is scale 0.
    Numeric {
        precision: Option<u16>,
        scale: Option<u16>,
    },
    Float,
    /// `TEXT`, `VARCHAR(n)`; `None` for no length (`TEXT`, bare `VARCHAR`, `VARCHAR(MAX)`).
    Text(Option<u32>),
    /// Fixed-width `CHAR(n)`/`bpchar`. Distinct from `Text` because it space-pads and compares
    /// trailing spaces differently under `=` vs `LIKE` — which the `like-without-wildcard`
    /// rewrite must respect. A bare `CHAR` is `None`: Postgres reads it as `CHAR(1)`, SQL
    /// Server's `CAST` as `CHAR(30)`, Calcite as unbounded, so it names no width.
    Char(Option<u32>),
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
        match dt {
            DataType::Numeric(n) | DataType::Decimal(n) | DataType::Dec(n) => {
                let (precision, scale) = match n {
                    ExactNumberInfo::None => (None, None),
                    ExactNumberInfo::Precision(p) => (u16::try_from(*p).ok(), Some(0)),
                    ExactNumberInfo::PrecisionAndScale(p, s) => {
                        (u16::try_from(*p).ok(), u16::try_from(*s).ok())
                    }
                };
                return Type::Numeric { precision, scale };
            }
            DataType::Char(len) | DataType::Character(len) => return Type::Char(char_len(len)),
            DataType::Varchar(len) | DataType::CharacterVarying(len) => {
                return Type::Text(char_len(len))
            }
            _ => {}
        }
        let s = dt.to_string().to_lowercase();
        // `signed`/`unsigned` are MySQL's integer cast targets (`CAST(x AS SIGNED)`); `INT
        // UNSIGNED` already matches `int`, so this only adds the bare cast forms.
        if s.contains("int") || s.contains("signed") {
            Type::Integer
        } else if s.contains("bool") {
            Type::Boolean
        } else if s.contains("numeric") || s.contains("decimal") {
            Type::Numeric {
                precision: None,
                scale: None,
            }
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
                Type::Char(None)
            } else {
                Type::Text(None)
            }
        } else {
            Type::Other(s)
        }
    }
}

fn char_len(len: &Option<CharacterLength>) -> Option<u32> {
    match len {
        Some(CharacterLength::IntegerLength { length, .. }) => u32::try_from(*length).ok(),
        _ => None,
    }
}
