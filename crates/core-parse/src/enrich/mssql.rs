//! SQL Server (T-SQL) specific copy for the rules that diverge here. Everything else comes from
//! [`super::common`]. Editing this file never affects another dialect.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => common::non_sargable(expr_index()),
        "function-on-indexed-column" => common::function_on_indexed(expr_index()),

        "leading-wildcard-like" => common::leading_wildcard(remedy(
            "Use a full-text index for word search",
            "If you need substring or word matching, use SQL Server full-text search.",
            "Create a full-text index and query it with `CONTAINS` or `FREETEXT`.",
            "Full-text search is indexed, unlike a leading-wildcard `LIKE` scan.",
            "CREATE FULLTEXT INDEX ON t(name) KEY INDEX pk_t",
        )),

        "large-in-list" => common::large_in_list(remedy(
            "Use a table-valued parameter",
            "Pass the values as a set and join them instead of inlining the list.",
            "Declare a table-valued parameter and `JOIN` on it, or `JOIN STRING_SPLIT(@ids, ',')` \
             for a delimited string.",
            "The statement stays small and the plan does not grow with the value count.",
            "JOIN STRING_SPLIT(@ids, ',') AS v ON t.id = v.value",
        )),

        "order-by-random" => common::order_by_random(
            "NEWID()",
            remedy(
                "Sample without a full sort",
                "Draw an approximate random sample instead of sorting every row.",
                "Replace `ORDER BY NEWID()` with `TABLESAMPLE (1 PERCENT)` when an approximate \
                 sample is acceptable.",
                "TABLESAMPLE reads a fraction of the pages instead of sorting the whole table.",
                "SELECT * FROM t TABLESAMPLE (1 PERCENT)",
            ),
        ),

        "risky-cast" => Parts {
            title: "Cast to a stricter type can fail on bad data".into(),
            what: f.message.clone(),
            why: "SQL Server raises a conversion error on the first row whose value is not valid \
                  for the target type. The data is not visible at analysis time, so this is \
                  informational."
                .into(),
            remedies: vec![remedy(
                "Validate the values, or use TRY_CAST",
                "Make sure every value converts, or use `TRY_CAST` to get NULL instead of an error.",
                "Validate the column before casting, change the column to the target type, or use \
                 `TRY_CAST` when a NULL on bad input is acceptable.",
                "Clean or typed data removes the failure, and `TRY_CAST` turns a hard error into a \
                 NULL.",
                "TRY_CAST(c AS int)",
            )],
        },

        "order-by-not-in-distinct-select" => common::order_by_not_in_distinct(
            "SQL Server rejects ordering by a column that isn't in the `SELECT DISTINCT` list, so \
             the query will not run.",
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
            "SQL Server rejects a non-grouped, non-aggregated column, so the query will not run.",
            remedy(
                "Group it or aggregate it",
                "Every SELECT column must be grouped or inside an aggregate.",
                "Add the column to `GROUP BY`, or wrap it in an aggregate such as `MAX(col)`.",
                "The query is then valid.",
                "SELECT dept, MAX(name) FROM emp GROUP BY dept",
            ),
        ),

        "string-numeric-compare" => common::string_numeric_compare(
            "SQL Server converts the text column to a number by type precedence, defeating its \
             index and erroring on any non-numeric value.",
            remedy(
                "Compare like types",
                "Quote the number, or store the column as a numeric type.",
                "Use `code = '123'` to compare as text, or make the column numeric.",
                "A same-type comparison avoids the conversion and uses the index.",
                "WHERE code = '123'",
            ),
        ),

        "join-type-mismatch" => common::join_type_mismatch(
            "SQL Server converts the lower-precedence side to match, defeating the index and \
             erroring on bad data.",
            remedy(
                "Make the join columns the same type",
                "Fix the schema, or cast one side explicitly.",
                "Align the column types (preferred), or cast in the join: \
                 `ON a.id = CAST(b.id AS int)`.",
                "Matching types let the join use the index instead of converting.",
                "ON a.id = CAST(b.id AS int)",
            ),
        ),

        "integer-division" => common::integer_division(
            "Integer divided by integer truncates (`5 / 2` is `2`), silently dropping the \
             remainder.",
            remedy(
                "Cast one side for a fractional result",
                "Divide as a decimal when you want the fraction.",
                "Cast one operand: `CAST(a AS decimal) / b` (or `a * 1.0 / b`).",
                "A decimal operand keeps the fractional part.",
                "SELECT CAST(a AS decimal) / b",
            ),
        ),

        "not-equals-excludes-null" => common::not_equals_excludes_null(remedy(
            "Use IS DISTINCT FROM (SQL Server 2022+)",
            "SQL Server 2022 added the null-safe inequality.",
            "Replace `col <> v` with `col IS DISTINCT FROM v` (on older versions add \
             `OR col IS NULL`).",
            "`IS DISTINCT FROM` treats NULL as a value, so NULL rows are kept.",
            "WHERE col IS DISTINCT FROM v",
        )),

        _ => return None,
    })
}

/// SQL Server cannot index an expression directly; a persisted computed column can be indexed.
fn expr_index() -> Remedy {
    remedy(
        "Or add a persisted computed column and index it",
        "SQL Server cannot index an expression directly, so materialize it.",
        "Add `ALTER TABLE t ADD email_lower AS lower(email) PERSISTED`, then index `email_lower`.",
        "The persisted computed column is indexable, so the predicate can seek.",
        "ALTER TABLE t ADD email_lower AS lower(email) PERSISTED",
    )
}
