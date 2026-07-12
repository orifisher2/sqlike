//! MySQL-specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => common::non_sargable(expr_index()),
        "function-on-indexed-column" => common::function_on_indexed(expr_index()),

        "leading-wildcard-like" => common::leading_wildcard(remedy(
            "Use a FULLTEXT index for word search",
            "If you need substring or word matching, use MySQL full-text search.",
            "Add a `FULLTEXT` index and query it with `MATCH(col) AGAINST(...)`, using an ngram \
             parser for substring matches.",
            "Full-text search is indexed, unlike a leading-wildcard `LIKE` scan.",
            "ALTER TABLE t ADD FULLTEXT(name)",
        )),

        "large-in-list" => common::large_in_list(remedy(
            "Join a derived table instead of inlining",
            "MySQL has no array parameter, so pass the values as one argument and join them.",
            "On MySQL 8 use `JSON_TABLE` over a JSON array argument, or load the values into a temp \
             table and `JOIN` on it.",
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

        "risky-cast" => Parts {
            title: "Cast silently coerces bad data".into(),
            what: f.message.clone(),
            why: "MySQL does not error on a bad cast. It coerces the value (`'abc'` becomes 0, an \
                  invalid date becomes NULL) with only a warning, hiding bad data. The data is not \
                  visible at analysis time, so this is informational."
                .into(),
            remedies: vec![remedy(
                "Validate the values, or store the proper type",
                "Catch bad values instead of letting them coerce to 0 or NULL.",
                "Validate the column before casting, change the column to the target type, or run \
                 in `STRICT` SQL mode so a bad cast errors instead of coercing.",
                "A typed column (or strict mode) surfaces bad data instead of silently zeroing it.",
                "ALTER TABLE t MODIFY c INT",
            )],
        },

        "like-on-numeric-column" => common::like_on_numeric(
            "MySQL implicitly casts the column to text, so the query runs but the cast is \
             non-sargable and full-scans the table.",
            remedy(
                "Compare like types",
                "Use a numeric comparison, or store the value as text.",
                "For a numeric match use `acct = 4001`; for a textual prefix match, store the \
                 column as text.",
                "A numeric comparison can use the index instead of scanning.",
                "WHERE acct = 4001",
            ),
        ),

        "order-by-not-in-distinct-select" => common::order_by_not_in_distinct(
            "MySQL runs the query, but the ordering depends on which duplicate row `DISTINCT` keeps \
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
            "Under the default `ONLY_FULL_GROUP_BY`, MySQL rejects a non-grouped, non-aggregated \
             column, so the query will not run.",
            remedy(
                "Group it or aggregate it",
                "Every SELECT column must be grouped or inside an aggregate.",
                "Add the column to `GROUP BY`, or wrap it in an aggregate such as `MAX(col)`.",
                "The query is then valid under ONLY_FULL_GROUP_BY.",
                "SELECT dept, MAX(name) FROM emp GROUP BY dept",
            ),
        ),

        "string-numeric-compare" => common::string_numeric_compare(
            "MySQL implicitly casts the column to a number, so the comparison runs but cannot use a \
             text index and full-scans the table.",
            remedy(
                "Compare like types",
                "Compare text to text so the index applies.",
                "Quote the number (`code = '123'`) so it compares as text against the indexed \
                 column.",
                "A text-to-text comparison can use the index instead of scanning.",
                "WHERE code = '123'",
            ),
        ),

        "join-type-mismatch" => common::join_type_mismatch(
            "MySQL coerces one side to match, so the join runs but cannot use the index on that \
             column and scans.",
            remedy(
                "Make the join columns the same type",
                "Fix the schema, or cast one side explicitly.",
                "Align the column types (preferred), or cast in the join: \
                 `ON a.id = CAST(b.id AS SIGNED)`.",
                "Matching types let the join use the index instead of scanning.",
                "ON a.id = CAST(b.id AS SIGNED)",
            ),
        ),

        "integer-division" => common::integer_division(
            "MySQL's `/` returns a decimal (`5 / 2` is `2.5`), so you get a fractional result where \
             other engines truncate; integer division is the `DIV` operator.",
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
            "MySQL has no `IS DISTINCT FROM`; negate the null-safe `<=>` instead.",
            "Replace `col <> v` with `NOT (col <=> v)` (or add `OR col IS NULL`).",
            "`<=>` treats NULL as a value, so negating it keeps NULL rows.",
            "WHERE NOT (col <=> v)",
        )),

        _ => return None,
    })
}

/// MySQL 8 supports functional key parts, so the expression can be indexed.
fn expr_index() -> Remedy {
    remedy(
        "Or add a functional index (MySQL 8+)",
        "Index the expression with a functional key part.",
        "`CREATE INDEX idx_lower_email ON t ((lower(email)))` (note the double parentheses).",
        "A MySQL 8 functional index makes the wrapped predicate seekable.",
        "CREATE INDEX idx_lower_email ON t ((lower(email)))",
    )
}
