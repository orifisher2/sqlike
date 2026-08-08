//! MariaDB-specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.
//!
//! Seeded from [`super::mysql`] in MA1, but only for claims that hold **by construction** — a
//! syntax or language-semantics fact of MariaDB. MySQL's copy for these rules asserts *engine
//! behaviour* that MariaDB has not been measured for, so they are deliberately left to
//! [`super::common`] until their verification phase supplies a measurement:
//!
//! - `risky-cast`, `string-numeric-compare`, `join-type-mismatch`, `like-on-numeric-column` — the
//!   coercion story (`'abc'` → 0, index defeated or not) — owed by **MA3a**.
//! - `select-non-grouped-column`, `order-by-not-in-distinct-select` — MariaDB has no
//!   `ONLY_FULL_GROUP_BY` in its default `sql_mode`, so it likely *runs* these (SQLite's story,
//!   not MySQL's), but that is unmeasured — owed by **MA3a**.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => common::non_sargable(expr_index()),
        "function-on-indexed-column" => common::function_on_indexed(expr_index()),

        "leading-wildcard-like" => common::leading_wildcard(remedy(
            "Use a FULLTEXT index for word search",
            "If you need word matching, use MariaDB full-text search.",
            "Add a `FULLTEXT` index and query it with `MATCH(col) AGAINST(...)`. It matches whole \
             words — MariaDB has no ngram parser, so true substring search needs a different \
             design (a reversed-string index, or a trigram table).",
            "Full-text search is indexed, unlike a leading-wildcard `LIKE` scan.",
            "ALTER TABLE t ADD FULLTEXT(name)",
        )),

        "large-in-list" => common::large_in_list(remedy(
            "Join a derived table instead of inlining",
            "MariaDB has no array parameter, so pass the values as one argument and join them.",
            "Use `JSON_TABLE` over a JSON array argument (MariaDB 10.6+), or load the values into \
             a temp table and `JOIN` on it.",
            "The statement stays small and the plan does not grow with the value count.",
            "JOIN JSON_TABLE(?, '$[*]' COLUMNS (id INT PATH '$')) AS v ON t.id = v.id",
        )),

        "order-by-random" => common::order_by_random(
            "RAND()",
            remedy(
                "Avoid the full sort for sampling",
                "Pick random rows without sorting the whole table.",
                "Filter on a random threshold such as `WHERE RAND() < 0.01`, or select random \
                 primary keys in the application, instead of `ORDER BY RAND()`.",
                "It avoids computing and sorting a random value for every row.",
                "WHERE RAND() < 0.01",
            ),
        ),

        "integer-division" => common::integer_division(
            "MariaDB's `/` returns a decimal (`5 / 2` is `2.5`), so you get a fractional result \
             where other engines truncate; integer division is the `DIV` operator.",
            remedy(
                "Use DIV for integer division",
                "Pick `/` for a decimal or `DIV` for an integer quotient, on purpose.",
                "Use `a DIV b` when you want integer division, or keep `/` for the decimal result.",
                "Being explicit avoids the wrong numeric type flowing downstream.",
                "SELECT a DIV b",
            ),
        ),

        "not-equals-excludes-null" => common::not_equals_excludes_null(remedy(
            "Negate the null-safe equal",
            "MariaDB has no `IS DISTINCT FROM`; negate the null-safe `<=>` instead.",
            "Replace `col <> v` with `NOT (col <=> v)` (or add `OR col IS NULL`).",
            "`<=>` treats NULL as a value, so negating it keeps NULL rows.",
            "WHERE NOT (col <=> v)",
        )),

        _ => return None,
    })
}

/// MariaDB has no functional key parts (MySQL's `((expr))` form is a syntax error here) — the
/// equivalent is a generated column carrying the expression, with an index on that column.
/// MA3d verifies this DDL executes and that the predicate then seeks.
fn expr_index() -> Remedy {
    remedy(
        "Or index a generated column",
        "MariaDB cannot index an expression directly; store it in a generated column and index \
         that.",
        "`ALTER TABLE t ADD email_lower VARCHAR(255) AS (lower(email)) VIRTUAL;` then \
         `CREATE INDEX idx_email_lower ON t (email_lower)`, and query the generated column.",
        "An indexed generated column makes the wrapped predicate seekable.",
        "ALTER TABLE t ADD email_lower VARCHAR(255) AS (lower(email)) VIRTUAL",
    )
}
