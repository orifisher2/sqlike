//! MariaDB-specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.
//!
//! Every claim here is either a syntax fact of MariaDB or a behaviour **measured on real
//! MariaDB 11.4** (MA3a, `crates/verify/tests/mariadb.rs`). The coercion entries in particular are
//! not inherited from MySQL: MariaDB was measured to defeat the index on
//! `string-numeric-compare` and `like-on-numeric-column` but to *keep* it on
//! `implicit-cast-in-filter`, and to run the grouping shapes MySQL rejects.

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

        // `common` says a B-tree simply cannot serve `<>`, which is true on Postgres and false
        // here: MariaDB range-scans everything-except. What makes the finding worth keeping is that
        // the range is normally the whole table — measured both ways, non-selective and skewed.
        "inequality-defeats-index" => Parts {
            title: "!= / <> usually matches almost every row".into(),
            what: f.message.clone(),
            why: "MariaDB can serve this from the index, as a range over everything except the \
                  excluded value, so the question is how many rows that leaves. Normally the \
                  excluded value is rare, almost every row qualifies, and reading the table is \
                  cheaper than searching the index. Where the excluded value is the common one, \
                  the index is used and the filter is selective."
                .into(),
            remedies: vec![remedy(
                "Rephrase as a positive set, or accept the scan",
                "If the allowed set is small and known, list what you want instead.",
                "Replace `status <> 'done'` with `status IN ('open', 'pending')` when the allowed \
                 values are known.",
                "An `IN` of the wanted values seeks a few index ranges instead of reading all of \
                 them.",
                "WHERE status IN ('open', 'pending')",
            )],
        },

        "risky-cast" => Parts {
            title: "Cast silently coerces bad data".into(),
            what: f.message.clone(),
            why: "MariaDB does not error on a bad cast. It coerces the value — \
                  `CAST('abc' AS SIGNED)` was measured returning 0 — with only a warning, hiding \
                  bad data. The data is not visible at analysis time, so this is informational."
                .into(),
            remedies: vec![remedy(
                "Validate the values, or store the proper type",
                "Catch bad values instead of letting them coerce to 0 or NULL.",
                "Validate the column before casting, or change the column to the target type so \
                 bad values are rejected on write.",
                "A typed column surfaces bad data instead of silently zeroing it.",
                "ALTER TABLE t MODIFY c INT",
            )],
        },

        "string-numeric-compare" => common::string_numeric_compare(
            "MariaDB implicitly casts the column to a number, so the comparison runs but cannot \
             use the text index and reads the whole table.",
            remedy(
                "Compare like types",
                "Compare text to text so the index applies.",
                "Quote the number (`code = '123'`) so it compares as text against the indexed \
                 column.",
                "A text-to-text comparison uses the index instead of scanning.",
                "WHERE code = '123'",
            ),
        ),

        "like-on-numeric-column" => common::like_on_numeric(
            "MariaDB implicitly casts the column to text, so the query runs but the cast is \
             non-sargable and the table is read in full.",
            remedy(
                "Compare like types",
                "Use a numeric comparison, or store the value as text.",
                "For a numeric match use `acct = 4001`; for a textual prefix match, store the \
                 column as text.",
                "A numeric comparison can use the index instead of scanning.",
                "WHERE acct = 4001",
            ),
        ),

        "join-type-mismatch" => common::join_type_mismatch(
            "MariaDB coerces one side to match, so the join runs instead of failing — and matches \
             rows you did not intend rather than telling you the types disagree.",
            remedy(
                "Make the join columns the same type",
                "Fix the schema, or cast one side explicitly.",
                "Align the column types (preferred), or cast in the join: \
                 `ON a.id = CAST(b.id AS SIGNED)`.",
                "Matching types remove the silent coercion and let the join use the index.",
                "ON a.id = CAST(b.id AS SIGNED)",
            ),
        ),

        "select-non-grouped-column" => common::select_non_grouped(
            "MariaDB's default `sql_mode` has no `ONLY_FULL_GROUP_BY`, so it runs the query and \
             returns the column's value from an arbitrary row in each group — measured, and \
             nondeterministic.",
            remedy(
                "Group it or aggregate it",
                "Make the value well-defined.",
                "Add the column to `GROUP BY`, or wrap it in an aggregate such as `MAX(col)`.",
                "The result no longer depends on which row the engine happens to pick.",
                "SELECT dept, MAX(name) FROM emp GROUP BY dept",
            ),
        ),

        "order-by-not-in-distinct-select" => common::order_by_not_in_distinct(
            "MariaDB runs the query, but the ordering depends on which duplicate row `DISTINCT` \
             keeps — so the order is effectively arbitrary.",
            remedy(
                "Add the column to the SELECT list",
                "Make the ordering well-defined.",
                "Add the ORDER BY column to the select list, or drop the DISTINCT if it isn't \
                 needed.",
                "The order no longer depends on which duplicate survives.",
                "SELECT DISTINCT a, b FROM t ORDER BY b",
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
