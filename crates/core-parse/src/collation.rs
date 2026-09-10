//! What a collation can and cannot do to a text value.
//!
//! A string's identity belongs to the collation, not to its bytes. `SELECT 'a' = 'A'` is **true** on
//! MySQL, MariaDB and SQL Server under their default configurations, and false on Postgres, SQLite
//! and DuckDB — all six measured on the real engines in
//! `varq-verify::collation_sensitive_literals`.
//!
//! Reading value-inequality off the bytes was a live false `Equivalent` in four separate places, so
//! the answer lives here once rather than being re-derived per crate.

use crate::Dialect;

/// Whether two string literals are values **no collation can equate**.
///
/// Deliberately narrow. ASCII alphanumerics only, which leaves case as the single mechanism that
/// could still bring them together — and `eq_ignore_ascii_case` has ruled that out. Everything the
/// restriction excludes is a way two different-looking strings can compare equal somewhere:
///
/// - **case** — `'a' = 'A'` on MySQL, MariaDB, SQL Server
/// - **accents** — MySQL's default collation is accent-insensitive, so `'e' = 'é'`
/// - **padding** — a `PAD SPACE` collation equates `'a'` with `'a '`
/// - **punctuation** — a collation may treat it as ignorable at primary strength
///
/// `'a'` and `'b'` stay distinct, which is what mutually-exclusive `CASE` arms and contradictory
/// pins rely on; `'a'` and `'A'` do not.
pub fn text_definitely_ne(x: &str, y: &str) -> bool {
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
    plain(x) && plain(y) && !x.eq_ignore_ascii_case(y)
}

/// Whether the dialect's **default** collation compares text exactly, so literals differing in bytes
/// differ in value.
///
/// Measured per engine, not inferred: `SELECT 'a' = 'A'` is false on Postgres, SQLite and DuckDB,
/// and true on MySQL, MariaDB and SQL Server.
///
/// This is the dialect's default only. A column declared `COLLATE` overrides it, and the schema
/// model does not yet carry that — see `docs/design-assumed-equivalence.md`.
pub fn text_compare_is_exact(dialect: Dialect) -> bool {
    matches!(
        dialect,
        Dialect::Postgres | Dialect::Sqlite | Dialect::Duckdb
    )
}

/// Two string literals that differ **only** by ASCII case — `'a'` and `'A'`, not `'a'` and `'b'`.
///
/// The complement of the case [`text_definitely_ne`] rules out, and the one shape where the answer
/// genuinely flips between engines rather than merely being unknown: a contradiction where text
/// compares exactly, and a pair of spellings that match the same rows where it does not.
pub fn differ_only_by_ascii_case(x: &str, y: &str) -> bool {
    x != y && x.is_ascii() && y.is_ascii() && x.eq_ignore_ascii_case(y)
}
