//! SQLite-specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => common::non_sargable(expr_index()),
        "function-on-indexed-column" => common::function_on_indexed(expr_index()),

        "leading-wildcard-like" => common::leading_wildcard(remedy(
            "Use an FTS5 virtual table for substring search",
            "If you need substring or token matching, use SQLite full-text search.",
            "Create an `fts5` virtual table over the column and query it with `MATCH`.",
            "FTS5 is indexed, unlike a leading-wildcard `LIKE` scan.",
            "CREATE VIRTUAL TABLE t_fts USING fts5(name)",
        )),

        "large-in-list" => common::large_in_list(remedy(
            "Join json_each over a parameter",
            "SQLite has no array parameter, so pass the values as one JSON argument and join them.",
            "Bind a JSON array and `JOIN json_each(?)` on its `value` column instead of inlining \
             the list.",
            "The statement stays small and the plan does not grow with the value count.",
            "JOIN json_each(?) AS v ON t.id = v.value",
        )),

        "order-by-random" => common::order_by_random(
            "random()",
            remedy(
                "Avoid the full sort for sampling",
                "Pick random rows without sorting the whole table.",
                "Filter on a random threshold, or select random rowids in the application, instead \
                 of `ORDER BY random()`.",
                "It avoids computing and sorting a random value for every row.",
                "WHERE abs(random()) % 100 < 1",
            ),
        ),

        "risky-cast" => Parts {
            title: "Cast silently coerces bad data".into(),
            what: f.message.clone(),
            why: "SQLite's `CAST` never errors. A non-numeric value becomes 0 \
                  (`CAST('abc' AS INTEGER)` is 0), hiding bad data. The data is not visible at \
                  analysis time, so this is informational."
                .into(),
            remedies: vec![remedy(
                "Validate the values",
                "Catch non-convertible values instead of letting them cast to 0.",
                "Validate the column before casting, or add a `CHECK` constraint so bad values \
                 cannot be stored.",
                "A validated column surfaces bad data instead of silently zeroing it.",
                "CHECK (typeof(c) = 'integer')",
            )],
        },

        "order-by-not-in-distinct-select" => common::order_by_not_in_distinct(
            "SQLite runs the query, but the ordering depends on which duplicate row `DISTINCT` keeps \
             — so the order is effectively arbitrary.",
            remedy(
                "Add the column to the SELECT list",
                "Make the ordering well-defined.",
                "Add the ORDER BY column to the select list, or drop the DISTINCT if it isn't \
                 needed.",
                "The order no longer depends on which duplicate survives.",
                "SELECT DISTINCT a, b FROM t ORDER BY b",
            ),
        ),

        "select-non-grouped-column" => common::select_non_grouped(
            "SQLite runs the query but returns the column's value from an arbitrary row in each \
             group, so the result is nondeterministic.",
            remedy(
                "Group it or aggregate it",
                "Make the value well-defined.",
                "Add the column to `GROUP BY`, or wrap it in an aggregate such as `MAX(col)`.",
                "The result no longer depends on which row SQLite happens to pick.",
                "SELECT dept, MAX(name) FROM emp GROUP BY dept",
            ),
        ),

        "string-numeric-compare" => common::string_numeric_compare(
            "SQLite applies column affinity, so the comparison usually runs, but comparing a text \
             column to a number may skip its index and scan.",
            remedy(
                "Compare like types",
                "Quote the number to compare with the column's text affinity.",
                "Use `code = '123'` so the comparison matches the column's affinity and its index.",
                "A same-affinity comparison can use the index.",
                "WHERE code = '123'",
            ),
        ),

        "join-type-mismatch" => common::join_type_mismatch(
            "SQLite's affinity coerces values, so the join runs, but mismatched types can skip the \
             index and scan.",
            remedy(
                "Make the join columns the same type",
                "Give the key columns matching affinity, or cast one side.",
                "Align the column declarations (preferred), or cast in the join: \
                 `ON a.id = CAST(b.id AS INTEGER)`.",
                "Matching affinity lets the join use the index.",
                "ON a.id = CAST(b.id AS INTEGER)",
            ),
        ),

        "integer-division" => common::integer_division(
            "Integer divided by integer truncates (`5 / 2` is `2`), silently dropping the \
             remainder.",
            remedy(
                "Multiply by 1.0 for a fractional result",
                "Force real division when you want the fraction.",
                "Use `a * 1.0 / b` (or `CAST(a AS REAL) / b`).",
                "A real operand keeps the fractional part.",
                "SELECT a * 1.0 / b",
            ),
        ),

        "not-equals-excludes-null" => common::not_equals_excludes_null(remedy(
            "Use IS NOT",
            "SQLite's `IS NOT` is null-safe.",
            "Replace `col <> v` with `col IS NOT v` (or add `OR col IS NULL`).",
            "`IS NOT` treats NULL as a value, so NULL rows are kept.",
            "WHERE col IS NOT v",
        )),

        _ => return None,
    })
}

/// SQLite supports indexes on expressions, so the wrapped predicate can be made seekable.
fn expr_index() -> Remedy {
    remedy(
        "Or add an expression index",
        "Index the expression itself.",
        "`CREATE INDEX idx_lower_email ON t (lower(email))`.",
        "SQLite can index an expression, so `lower(email) = 'x'` seeks the index.",
        "CREATE INDEX idx_lower_email ON t (lower(email))",
    )
}
