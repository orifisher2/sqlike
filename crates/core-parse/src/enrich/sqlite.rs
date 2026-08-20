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


        // QA-1 / R-5b. Same correction as Postgres, different plan node: `EXPLAIN QUERY PLAN`
        // reports `MULTI-INDEX OR`, one indexed search per branch per outer row. The split wins
        // only against a near-unique join key and loses from a handful of matches upward, so
        // `or_split_pays_off` withholds the fix here too.
        "or-in-join-on" => Parts {
            title: "OR in the JOIN ON, which SQLite can still index".into(),
            what: "A join `ON` condition contains `OR`, so the join key is no longer a single \
                   equality."
                .into(),
            why: "An `OR` rules out a merge-style join, but SQLite can still reach both sides \
                  through their indexes: `EXPLAIN QUERY PLAN` shows a multi-index OR when each \
                  branch's column is indexed. Splitting the join into one branch per arm helps \
                  only when each row matches a handful of rows on the other side, and loses \
                  otherwise, so it is not offered as a fix."
                .into(),
            remedies: vec![remedy(
                "Check the plan before restructuring",
                "Confirm what the join actually does before rewriting it.",
                "Run `EXPLAIN QUERY PLAN` and look for `MULTI-INDEX OR`. If it is there, both \
                 branches are already indexed. If you see a full scan, index each branch column.",
                "Indexing the branch columns is what makes this join cheap here. The union rewrite \
                 adds a second join and a guard that cannot use an index.",
                "EXPLAIN QUERY PLAN SELECT ... FROM a JOIN b ON b.x = a.x OR b.y = a.y",
            )],
        },

        // QA-1 / R-5. `common` says the two forms match the same rows. On SQLite they do not:
        // `LIKE` is case-insensitive for ASCII and `=` is not, measured 2 rows against 1. The fix
        // is withheld here (`fix_preserves_results`), so the copy must not recommend the swap as
        // if it were free.
        "like-without-wildcard" => Parts {
            title: "LIKE with no wildcard is a case-insensitive match".into(),
            what: "The `LIKE` pattern contains no `%` or `_`, so it matches one string — but \
                   SQLite matches `LIKE` without regard to ASCII case."
                .into(),
            why: "This is not the same test as `=`, which is case-sensitive: a row stored as \
                  `'ABC'` matches `LIKE 'abc'` and does not match `= 'abc'`. It also cannot use an \
                  index on the column, so it scans. Switching to `=` is worth doing when exact \
                  matching is what you meant, but it will drop the case-insensitive matches."
                .into(),
            remedies: vec![remedy(
                "Decide which match you meant, then say it",
                "The two readings differ here, so the choice has to be explicit.",
                "For an exact match write `name = 'term'` and expect rows differing only by case \
                 to disappear. To keep matching case-insensitively, say so with \
                 `name = 'term' COLLATE NOCASE`, which can also use a `COLLATE NOCASE` index.",
                "Either form states the intent plainly, and the collate form can be indexed, which \
                 the wildcard-free `LIKE` cannot.",
                "WHERE name = 'term' COLLATE NOCASE",
            )],
        },


        // QA-1 / R-4. `common` says choosing the page first "does far less work". Not here:
        // `EXPLAIN QUERY PLAN` shows the naive form is already `SCAN d USING INDEX` with a per-row
        // key lookup, so SQLite walks the index without materialising the rows it skips. Measured
        // across 12 cells (1 and 5 joins x 200 B and 1 KB rows x offsets 0 / 50k / 300k) the
        // deferral never won: best of twelve was 1.11x, and wider rows made it worse, the opposite
        // of the mechanism's premise. `defer_pays_off` withholds the fix, so the copy must not
        // recommend it.
        "deferred-join-pagination" => Parts {
            title: "Paginated query, already served by an index walk".into(),
            what: "A paginated query joins extra tables before applying `LIMIT`, so the join runs \
                   for rows that are then discarded."
                .into(),
            why: "On other engines the fix is to pick the page's keys first and join to those. \
                  SQLite gains little from it, because it already reaches the page by walking the \
                  ordering index and fetching each row by key rather than building the skipped \
                  rows. Measured at or below break-even at every offset and row width tried, so \
                  the rewrite is not offered here. A deep `OFFSET` is still linear in the rows \
                  skipped: that cost is the offset itself, not the joins."
                .into(),
            remedies: vec![remedy(
                "Index the ordering, then page by key instead of by offset",
                "Remove the skipped-row cost rather than moving the joins.",
                "Make sure the `ORDER BY` columns are indexed so the walk is an index scan, then \
                 replace `OFFSET` with a `WHERE` on the last key of the previous page.",
                "Keyset pagination reads only the page, so cost stays flat as the offset grows, \
                 while `OFFSET` walks every row before it.",
                "WHERE (created, id) < (:last_created, :last_id) ORDER BY created DESC, id DESC \
                 LIMIT 20",
            )],
        },

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
