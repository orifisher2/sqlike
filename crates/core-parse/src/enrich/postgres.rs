//! Postgres-specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => common::non_sargable(expr_index()),
        "function-on-indexed-column" => common::function_on_indexed(expr_index()),

        "leading-wildcard-like" => common::leading_wildcard(remedy(
            "Use a trigram index for substring search",
            "If you genuinely need `'%term%'`, index the column for substring matching.",
            "Create a `pg_trgm` GIN index on the column.",
            "The trigram index makes arbitrary substring matches seekable instead of a full scan.",
            "CREATE INDEX ON t USING gin (name gin_trgm_ops)",
        )),

        "large-in-list" => common::large_in_list(remedy(
            "Pass an array parameter",
            "Replace the inline list with a single array parameter.",
            "Use `= ANY($1)` with an array argument instead of `IN (1, 2, 3, ...)`.",
            "One bound parameter keeps the statement small and the plan stable no matter how many \
             values you pass.",
            "WHERE id = ANY($1)",
        )),

        "order-by-random" => common::order_by_random(
            "random()",
            remedy(
                "Sample without a full sort",
                "Draw an approximate random sample instead of sorting every row.",
                "Use `TABLESAMPLE BERNOULLI (1)` for an approximate 1% sample when exactness is not \
                 required.",
                "TABLESAMPLE reads a fraction of the pages instead of sorting the whole table.",
                "SELECT * FROM t TABLESAMPLE BERNOULLI (1)",
            ),
        ),

        "risky-cast" => Parts {
            title: "Cast to a stricter type can fail on bad data".into(),
            what: f.message.clone(),
            why: "Postgres raises an error on the first row whose value is not valid for the target \
                  type. The data is not visible at analysis time, so this is informational."
                .into(),
            remedies: vec![remedy(
                "Validate the values, or store the proper type",
                "Make sure every value converts, or keep the column in its real type.",
                "Validate the column before casting, or change the column to the target type so bad \
                 values cannot get in.",
                "A clean (or already-typed) column removes the runtime cast and its failure mode.",
                "ALTER TABLE t ALTER COLUMN c TYPE integer USING c::integer",
            )],
        },

        "like-on-numeric-column" => common::like_on_numeric(
            "Postgres has no operator to match `LIKE` against a number, so this raises an error and \
             the query will not run.",
            remedy(
                "Compare like types",
                "Use a numeric comparison, or store the value as text.",
                "For a numeric match use `acct = 4001`; for a textual prefix match, store the \
                 column as text.",
                "Matching types let the query run and use the index.",
                "WHERE acct = 4001",
            ),
        ),

        "order-by-not-in-distinct-select" => common::order_by_not_in_distinct(
            "Postgres rejects ordering by a column that isn't in the `SELECT DISTINCT` list, so the \
             query will not run.",
            remedy(
                "Add the column to the SELECT list",
                "DISTINCT must see the ordering column.",
                "Add the ORDER BY column to the select list, or drop the DISTINCT if it isn't \
                 needed.",
                "The query becomes well-defined and valid on every engine.",
                "SELECT DISTINCT a, b FROM t ORDER BY b",
            ),
        ),

        "select-non-grouped-column" => common::select_non_grouped(
            "Postgres rejects a non-grouped, non-aggregated column, so the query will not run \
             (unless the column is functionally dependent on a grouped primary key).",
            remedy(
                "Group it or aggregate it",
                "Every SELECT column must be grouped or inside an aggregate.",
                "Add the column to `GROUP BY`, or wrap it in an aggregate such as `MAX(col)`.",
                "The query is then valid and its result is well-defined.",
                "SELECT dept, MAX(name) FROM emp GROUP BY dept",
            ),
        ),

        "string-numeric-compare" => common::string_numeric_compare(
            "Postgres has no operator to compare text to a number, so this raises an error and the \
             query will not run.",
            remedy(
                "Compare like types",
                "Compare text to text, or make the column numeric.",
                "Quote the number to compare as text (`code = '123'`), or store and cast the column \
                 as a numeric type.",
                "Matching types let the comparison run and use the index.",
                "WHERE code = '123'",
            ),
        ),

        "join-type-mismatch" => common::join_type_mismatch(
            "Postgres has no operator to compare the two types, so the join raises an error and \
             will not run.",
            remedy(
                "Make the join columns the same type",
                "Fix the schema, or cast one side explicitly.",
                "Align the column types (preferred), or cast in the join: `ON a.id = b.id::int`.",
                "Matching types let the join run and use the index.",
                "ON a.id = CAST(b.id AS integer)",
            ),
        ),

        "integer-division" => common::integer_division(
            "Integer divided by integer truncates toward zero (`5 / 2` is `2`), silently dropping \
             the remainder.",
            remedy(
                "Cast one side to numeric for a fractional result",
                "Divide as numeric when you want the fraction.",
                "Cast one operand: `a::numeric / b` (or `a * 1.0 / b`).",
                "A numeric operand keeps the fractional part.",
                "SELECT a::numeric / b",
            ),
        ),

        "not-equals-excludes-null" => common::not_equals_excludes_null(remedy(
            "Use IS DISTINCT FROM",
            "Compare with the null-safe inequality so NULL rows are kept.",
            "Replace `col <> v` with `col IS DISTINCT FROM v` (or add `OR col IS NULL`).",
            "`IS DISTINCT FROM` treats NULL as an ordinary value, so NULL rows are included.",
            "WHERE col IS DISTINCT FROM v",
        )),

        _ => return None,
    })
}

/// Postgres can index an expression directly, so the function-wrapped predicate becomes seekable.
fn expr_index() -> Remedy {
    remedy(
        "Or add an expression index",
        "Index the expression itself so the wrapped predicate is seekable.",
        "`CREATE INDEX ON t (lower(email))` for the exact expression in the predicate.",
        "Postgres can index an expression, so `lower(email) = 'x'` seeks the index.",
        "CREATE INDEX ON t (lower(email))",
    )
}
