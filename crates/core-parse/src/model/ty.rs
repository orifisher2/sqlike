//! SQL types, coarse-grained to what analysis needs.
//!
//! Finer distinctions (integer widths, timezone-awareness, array element types)
//! are added when a rule actually depends on them. `Unknown` is the honest default
//! before schema-aware resolution fills real types in (F4). The exact-numeric and
//! character widths are kept because a cast to a wider type of the same kind holds
//! every value of the column and reads as the column (`docs/phase-tw1-type-widths.md`);
//! two widths are two types, so the derived equality tells them apart.

use crate::dialect::Dialect;
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
            // A column that declares no type: SQLite allows it outright, and T-SQL's computed
            // column has no type to declare. `Other("")` would be a named type whose name is
            // empty; this is the absence of one.
            DataType::Unspecified => return Type::Unknown,
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
        if MISREAD.contains(&s.split(['(', ' ']).next().unwrap_or_default()) {
            return Type::Other(s);
        }
        // `signed`/`unsigned` are MySQL's integer cast targets (`CAST(x AS SIGNED)`); `INT
        // UNSIGNED` already matches `int`, so this only adds the bare cast forms. Matched whole:
        // `DECIMAL(10, 2) UNSIGNED` is a decimal.
        if s.contains("int") || matches!(s.as_str(), "signed" | "unsigned") {
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
        } else if s.contains("timestamp") || s.contains("datetime") {
            // MySQL's `DATETIME` has a time part; matching `date` below would drop it.
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

/// Built-in names the spelling match below would read as the wrong kind: an `interval` or a `point`
/// is not an integer, a range of dates is not a date (`docs/phase-ct1-column-types.md`).
const MISREAD: &[&str] = &[
    "datemultirange",
    "daterange",
    "int2vector",
    "int4multirange",
    "int4range",
    "int8multirange",
    "int8range",
    "interval",
    "jsonpath",
    "multipoint",
    "point",
];

/// A type name with its arguments, array brackets and quoting removed, as the measured lists in
/// `data/types/` spell it: `[nvarchar](50)` is `nvarchar`, `TIMESTAMP(3) WITH TIME ZONE` is
/// `timestamp with time zone`. MySQL and MariaDB also drop the `UNSIGNED` modifier, which follows
/// any numeric type there.
pub fn base_name(dt: &ast::DataType, dialect: Dialect) -> String {
    let rendered = dt.to_string().to_lowercase();
    let rendered = rendered.trim_end_matches("[]");
    let mut out = String::new();
    let mut depth = 0;
    for c in rendered.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            '"' | '`' | '[' | ']' => {}
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let mut words: Vec<&str> = out.split_whitespace().collect();
    if matches!(dialect, Dialect::Mysql | Dialect::Mariadb)
        && words.len() > 1
        && words.last() == Some(&"unsigned")
    {
        words.pop();
    }
    words.join(" ")
}

/// Whether `base` (a [`base_name`]) is a type the engine has built in, as measured on the engine by
/// `crates/verify/tests/type_lists.rs`. SQLite accepts any name.
pub fn is_builtin(dialect: Dialect, base: &str) -> bool {
    let list = match dialect {
        Dialect::Sqlite => return true,
        Dialect::Postgres => include_str!("../../data/types/postgres.txt"),
        Dialect::Mssql => include_str!("../../data/types/mssql.txt"),
        Dialect::Mysql => include_str!("../../data/types/mysql.txt"),
        Dialect::Mariadb => include_str!("../../data/types/mariadb.txt"),
        Dialect::Duckdb => include_str!("../../data/types/duckdb.txt"),
    };
    list.lines().any(|l| l == base)
}

/// The engines that have `name` built in, in a fixed order. SQLite, which accepts any name, is not
/// counted: it says nothing about where a type comes from.
pub fn engines_with(name: &str) -> Vec<Dialect> {
    [
        Dialect::Postgres,
        Dialect::Mssql,
        Dialect::Mysql,
        Dialect::Mariadb,
        Dialect::Duckdb,
    ]
    .into_iter()
    .filter(|d| is_builtin(*d, name))
    .collect()
}

/// Whether `name` is a type name any engine has built in, or a Postgres extension type: public
/// vocabulary, so the tokenizer keeps it as written (CT1b) and the server can tell `[nvarchar]` from
/// a type the user declared.
pub fn is_public_type_name(name: &str) -> bool {
    !engines_with(name).is_empty() || is_postgres_extension_type(name)
}

/// Whether `name` is a type a Postgres extension provides (`vector`, `citext`, PostGIS's
/// `geometry`), measured by `crates/verify/tests/type_lists.rs`. Not built in, but a schema that
/// uses one is ordinary, so it is never reported as unknown.
pub fn is_postgres_extension_type(name: &str) -> bool {
    include_str!("../../data/types/postgres_extensions.txt")
        .lines()
        .any(|l| l.split('\t').next() == Some(name))
}

fn char_len(len: &Option<CharacterLength>) -> Option<u32> {
    match len {
        Some(CharacterLength::IntegerLength { length, .. }) => u32::try_from(*length).ok(),
        _ => None,
    }
}
