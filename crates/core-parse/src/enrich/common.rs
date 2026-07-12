//! Default, dialect-agnostic copy for the hand-written batch, plus the shared skeletons the
//! dialect modules fill in (`leading_wildcard`, `large_in_list`, `order_by_random`). Rules not
//! handled here fall back to [`super::derive`].

use super::{apply_remedy, remedy, resolve_remedy, Finding, Parts, Remedy};

/// Hand-written rich parts for a finding, or `None` to fall back to a dialect override / derive.
pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "select-star" => Parts {
            title: "SELECT * fetches every column".into(),
            what: "The query uses `*` instead of naming the columns it needs.".into(),
            why: "It reads and ships columns the query never uses, can rule out an index-only scan \
                  (the planner has to visit the table for the extra columns), and silently changes \
                  shape when a column is added, dropped, or reordered."
                .into(),
            remedies: vec![remedy(
                "List the columns explicitly",
                "Name only the columns the query actually reads.",
                "Replace `*` with the column list, for example `id, email`.",
                "The planner reads only what is needed and the result stays stable across schema \
                 changes.",
                "SELECT id, email FROM users",
            )],
        },

        "not-in-subquery" => Parts {
            title: "NOT IN with a subquery can return no rows".into(),
            what: "`NOT IN` compares each row against every value the subquery returns.".into(),
            why: "If the subquery returns even one NULL, the comparison is never true and the whole \
                  query returns zero rows. The bug stays hidden until the data contains a NULL."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Rewrite as NOT EXISTS",
                "A correlated anti-join that NULLs do not affect.",
                "Replace `x NOT IN (SELECT y ...)` with `NOT EXISTS (SELECT 1 ... WHERE y = x)`.",
                "Anti-join semantics ignore NULLs, so the result is correct whatever the data holds.",
                "SELECT a.id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.bid = a.id)",
            )],
        },

        "redundant-or-chain" => Parts {
            title: "Repeated OR on one column".into(),
            what: "Several `col = value` tests on the same column are chained with OR.".into(),
            why: "It reads worse than a single `IN` list and, on some engines, the planner handles \
                  a long OR chain less efficiently than the equivalent `IN`."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Collapse into an IN list",
                "One `col IN (...)` is shorter and optimizes consistently.",
                "Rewrite `a = 1 OR a = 2 OR a = 3` as `a IN (1, 2, 3)`.",
                "`IN` is the canonical form for set membership and the planner handles it directly.",
                "WHERE a IN (1, 2, 3)",
            )],
        },

        "or-across-tables" => Parts {
            title: "OR spans two tables, so no index applies".into(),
            what: "A `WHERE` predicate ORs conditions on different tables, such as \
                   `a.x = 1 OR b.y = 2`."
                .into(),
            why: "Neither table's index can satisfy an OR that also depends on the other table, so \
                  the planner falls back to scanning."
                .into(),
            remedies: vec![remedy(
                "Split into a UNION",
                "Run one indexable branch per table and union the results.",
                "Rewrite the OR as two queries joined by `UNION`, one filtering `a.x = 1` and one \
                 filtering `b.y = 2`.",
                "Each branch now filters on a single table and can use that table's index.",
                "SELECT ... WHERE a.x = 1\nUNION\nSELECT ... WHERE b.y = 2",
            )],
        },

        "offset-pagination" => Parts {
            title: "A large OFFSET scans and throws away rows".into(),
            what: "Pagination uses `OFFSET n`, which the database reaches by reading and discarding \
                   the first `n` rows."
                .into(),
            why: "Deep pages get slower in a straight line. Page 1000 reads a thousand times the \
                  rows of page 1 before it returns anything."
                .into(),
            remedies: vec![remedy(
                "Use keyset (seek) pagination",
                "Remember the last row's key and filter past it instead of counting an offset.",
                "Replace `ORDER BY id LIMIT 20 OFFSET n` with `WHERE id > :last_id ORDER BY id \
                 LIMIT 20`.",
                "The index jumps straight to the next page, so every page costs the same.",
                "SELECT ... WHERE id > :last_id ORDER BY id LIMIT 20",
            )],
        },

        "like-without-wildcard" => Parts {
            title: "LIKE with no wildcard is just equality".into(),
            what: "The `LIKE` pattern contains no `%` or `_`, so it matches one exact string.".into(),
            why: "`LIKE` without a wildcard does the work of `=` but reads as a pattern match, which \
                  misleads the next reader and can skip a plain index on some engines."
                .into(),
            remedies: vec![remedy(
                "Replace LIKE with =",
                "Use `=` because the pattern has no wildcard.",
                "Rewrite `name LIKE 'term'` as `name = 'term'`.",
                "`=` states the exact match plainly and uses a normal equality index.",
                "WHERE name = 'term'",
            )],
        },

        "cartesian-join" => Parts {
            title: "Join has no condition (Cartesian product)".into(),
            what: "Two tables are joined with nothing relating them, so every row of one pairs with \
                   every row of the other."
                .into(),
            why: "The result row count is the product of the two tables. It explodes as they grow \
                  and is almost never what was intended."
                .into(),
            remedies: vec![remedy(
                "Add the join condition",
                "Relate the tables on their key columns.",
                "Add `ON a.id = b.a_id` (or the matching `WHERE`) so each row pairs with its \
                 counterpart.",
                "The join returns matched rows instead of every combination.",
                "FROM a JOIN b ON a.id = b.a_id",
            )],
        },

        "limit-without-order-by" => Parts {
            title: "LIMIT without ORDER BY returns arbitrary rows".into(),
            what: "The query uses `LIMIT` but no `ORDER BY`, so which rows come back is undefined."
                .into(),
            why: "Without an order the engine may return any rows, and the set can change between \
                  runs as the data or plan shifts. `LIMIT` bounds the count, not which rows."
                .into(),
            remedies: vec![remedy(
                "Add an ORDER BY on a unique key",
                "Order by a column (or set of columns) that uniquely identifies a row before \
                 limiting.",
                "Add `ORDER BY id` on a unique or primary key before the `LIMIT`.",
                "The result becomes deterministic and stable across runs.",
                "SELECT ... ORDER BY id LIMIT 20",
            )],
        },

        "unknown-column" | "unknown-table" | "ambiguous-column" | "ambiguous-table"
        | "unknown-table-alias" => Parts {
            title: resolve_title(&f.rule).into(),
            what: f.message.clone(),
            why: "The query references a name the schema cannot resolve unambiguously, so it will \
                  not run as written."
                .into(),
            remedies: f
                .suggestion
                .as_deref()
                .map(|s| vec![resolve_remedy(s)])
                .unwrap_or_default(),
        },

        "float-equality" => Parts {
            title: "Comparing floats with = is unreliable".into(),
            what: "A floating-point value is compared with `=` (or `<>`).".into(),
            why: "Floats are stored approximately, so two values that look equal can differ in the \
                  last bits and `=` returns false. The result is non-portable across platforms."
                .into(),
            remedies: vec![remedy(
                "Compare within a tolerance",
                "Test that the difference is smaller than a small epsilon, not exact equality.",
                "Replace `x = 1.1` with `abs(x - 1.1) < 1e-9`.",
                "A tolerance absorbs the representation error, so the comparison is stable.",
                "WHERE abs(x - 1.1) < 1e-9",
            )],
        },

        "self-comparison" => Parts {
            title: "A column is compared to itself".into(),
            what: "A predicate compares a column to itself, such as `a = a`.".into(),
            why: "It is always true (or always unknown when the column is NULL), so it filters \
                  nothing or everything. Usually a typo for a different column."
                .into(),
            remedies: vec![remedy(
                "Compare the intended columns",
                "Replace one side with the column you meant.",
                "Change `a.x = a.x` to the real comparison, for example `a.x = b.x`.",
                "The predicate then filters as intended.",
                "WHERE a.x = b.x",
            )],
        },

        "reversed-between" => Parts {
            title: "BETWEEN bounds are reversed".into(),
            what: "A `BETWEEN low AND high` has the larger value first, so the range is empty.".into(),
            why: "`BETWEEN x AND y` matches only when `x <= value <= y`. With the bounds swapped no \
                  row can match, so the query silently returns nothing."
                .into(),
            remedies: vec![remedy(
                "Swap the bounds",
                "Put the smaller value first.",
                "Rewrite `BETWEEN 100 AND 10` as `BETWEEN 10 AND 100`.",
                "The range is non-empty and matches the intended values.",
                "WHERE n BETWEEN 10 AND 100",
            )],
        },

        "left-join-nullified" => Parts {
            title: "WHERE on a LEFT JOIN makes it an inner join".into(),
            what: "A `LEFT JOIN`'s right-table column is filtered in `WHERE` with a non-null test."
                .into(),
            why: "Unmatched left rows have NULL on the right side, and the `WHERE` test drops them, \
                  so the LEFT JOIN quietly behaves like an INNER JOIN."
                .into(),
            remedies: vec![remedy(
                "Move the condition into the ON clause",
                "Filter the right table in the join condition so unmatched left rows survive.",
                "Move `b.status = 'x'` from `WHERE` into the `ON` clause (or test `IS NULL` if you \
                 want the unmatched rows).",
                "The outer rows are kept and the join stays a LEFT JOIN.",
                "LEFT JOIN b ON b.a_id = a.id AND b.status = 'x'",
            )],
        },

        "having-without-aggregate" => Parts {
            title: "HAVING without an aggregate is just WHERE".into(),
            what: "A `HAVING` clause uses no aggregate function, so it filters individual rows."
                .into(),
            why: "`HAVING` is for filtering groups after aggregation. With no aggregate it does the \
                  work of `WHERE` but runs later and reads as if it filtered groups."
                .into(),
            remedies: vec![remedy(
                "Move the condition to WHERE",
                "Filter rows in `WHERE`, before grouping.",
                "Move the non-aggregate predicate from `HAVING` into `WHERE`.",
                "It filters earlier in the pipeline and reads clearly.",
                "WHERE status = 'active' GROUP BY user_id",
            )],
        },

        "not-in-nullable-column" => Parts {
            title: "NOT IN over a nullable column can return nothing".into(),
            what: "`NOT IN` is used with a set whose column can be NULL.".into(),
            why: "If any value in the set is NULL, `NOT IN` is never true and the query silently \
                  returns zero rows. The same NULL trap as `NOT IN (subquery)`."
                .into(),
            remedies: vec![remedy(
                "Rewrite as NOT EXISTS",
                "Use an anti-join that NULLs do not affect.",
                "Replace `x NOT IN (...)` with `NOT EXISTS (SELECT 1 ... WHERE ... = x)`, or exclude \
                 NULLs from the set.",
                "Anti-join semantics ignore NULLs, so the result is correct.",
                "WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.id = a.id)",
            )],
        },

        "unconditional-delete" => Parts {
            title: "DELETE with no WHERE empties the table".into(),
            what: "A `DELETE` has no `WHERE`, so it removes every row.".into(),
            why: "It deletes the entire table, often by accident, and is hard to undo outside a \
                  transaction."
                .into(),
            remedies: vec![remedy(
                "Add a WHERE clause",
                "Restrict the delete to the rows you mean.",
                "Add a `WHERE` that selects only the target rows, for example `WHERE id = :id`.",
                "Only the intended rows are removed.",
                "DELETE FROM t WHERE id = :id",
            )],
        },

        "unconditional-update" => Parts {
            title: "UPDATE with no WHERE rewrites every row".into(),
            what: "An `UPDATE` has no `WHERE`, so it changes every row.".into(),
            why: "It overwrites the whole table, usually by accident, and is expensive and hard to \
                  undo outside a transaction."
                .into(),
            remedies: vec![remedy(
                "Add a WHERE clause",
                "Restrict the update to the rows you mean.",
                "Add a `WHERE` that selects only the target rows, for example `WHERE id = :id`.",
                "Only the intended rows change.",
                "UPDATE t SET status = 'x' WHERE id = :id",
            )],
        },

        "dml-not-by-key" => Parts {
            title: "DELETE/UPDATE not filtered by the primary key".into(),
            what: "A `DELETE` or `UPDATE` filters on a non-key column, so it may affect more rows \
                   than expected."
                .into(),
            why: "Without a unique key in the filter the statement can touch many rows, and a typo \
                  can hit the wrong set. Targeting by key is precise and predictable."
                .into(),
            remedies: vec![remedy(
                "Filter by the primary key",
                "Identify the row by its key when you mean a single row.",
                "Add the primary key to the `WHERE`, for example `WHERE id = :id`.",
                "The statement affects exactly the intended row.",
                "UPDATE t SET status = 'x' WHERE id = :id",
            )],
        },

        "destructive-statement" => Parts {
            title: "TRUNCATE/DROP discards data irreversibly".into(),
            what: "A `TRUNCATE` empties a table or a `DROP` removes it entirely.".into(),
            why: "Both throw away data and cannot be undone outside a transaction. Fine for a \
                  deliberate migration, dangerous by accident, so this is informational."
                .into(),
            remedies: vec![remedy(
                "Confirm this is intentional",
                "Run destructive DDL deliberately, ideally inside a transaction or a reviewed \
                 migration.",
                "Wrap it in a transaction you can roll back, or suppress this finding where the \
                 destruction is intended.",
                "A transaction or migration step makes the intent explicit and recoverable.",
                "BEGIN; TRUNCATE t; -- verify, then COMMIT",
            )],
        },

        "equals-null" => Parts {
            title: "Comparing to NULL with = matches nothing".into(),
            what: "A predicate uses `= NULL` or `<> NULL`.".into(),
            why: "Any comparison to NULL with `=`/`<>` is unknown, never true, so it matches no \
                  rows. The intended test is `IS NULL` / `IS NOT NULL`."
                .into(),
            remedies: vec![remedy(
                "Use IS NULL / IS NOT NULL",
                "Test for NULL with the null-aware operators.",
                "Replace `x = NULL` with `x IS NULL`, and `x <> NULL` with `x IS NOT NULL`.",
                "`IS NULL` is the correct null test and matches the rows you mean.",
                "WHERE x IS NULL",
            )],
        },

        "inequality-defeats-index" => Parts {
            title: "!= / <> cannot use the index".into(),
            what: "A filter uses `<>` (or `NOT`) on a column, which a B-tree index cannot seek."
                .into(),
            why: "A B-tree finds equal or ranged values, not everything-except. The planner scans \
                  the table to evaluate the inequality."
                .into(),
            remedies: vec![remedy(
                "Rephrase as a positive set, or accept the scan",
                "If the allowed set is small and known, list what you want instead.",
                "Replace `status <> 'done'` with `status IN ('open', 'pending')` when the allowed \
                 values are known.",
                "An `IN` of the wanted values can seek the index, unlike `<>`.",
                "WHERE status IN ('open', 'pending')",
            )],
        },

        "or-defeats-index" => Parts {
            title: "OR on different columns defeats the index".into(),
            what: "A `WHERE` ORs conditions on different columns, so no single index covers the \
                   predicate."
                .into(),
            why: "An index is ordered by its columns; an OR across columns forces the planner to \
                  scan or to combine indexes, usually slower than a focused query."
                .into(),
            remedies: vec![remedy(
                "Split into a UNION, or index each branch",
                "Run one indexable branch per column and union the results.",
                "Rewrite `a = 1 OR b = 2` as two queries joined by `UNION`, each filtering one \
                 column.",
                "Each branch filters on one column and can use that column's index.",
                "SELECT ... WHERE a = 1\nUNION\nSELECT ... WHERE b = 2",
            )],
        },

        "index-prefix-mismatch" => Parts {
            title: "Query skips the index's leading column".into(),
            what: "A composite index exists, but the query filters on a non-leading column, so the \
                   index cannot be used."
                .into(),
            why: "A composite index is ordered left to right. Without the leading column in the \
                  filter the engine cannot seek into it and scans."
                .into(),
            remedies: vec![remedy(
                "Match the index prefix, or add a matching index",
                "Filter on the leading column too, or index the column you actually filter.",
                "Add the index's first column to the `WHERE`, or `CREATE INDEX t_b ON t(b)` for the \
                 column you filter.",
                "The filter then lines up with an index the engine can seek.",
                "CREATE INDEX t_b ON t(b)",
            )],
        },

        "unindexed-filter" => Parts {
            title: "Filter on a column with no index".into(),
            what: "A selective `WHERE` filters a column that has no index, so the engine scans the \
                   table."
                .into(),
            why: "Without an index the engine reads every row to find the matches. On a large table \
                  that is slow for a selective filter."
                .into(),
            remedies: vec![remedy(
                "Add an index on the filtered column",
                "Index the column the query filters on.",
                "Create an index on the filtered column, for example on `status`.",
                "The engine seeks to the matching rows instead of scanning.",
                "CREATE INDEX t_status ON t(status)",
            )],
        },

        "unindexed-join-key" => Parts {
            title: "Join key has no index".into(),
            what: "A join matches on a column with no index on the joined side, forcing a lookup \
                   scan."
                .into(),
            why: "Without an index on the join key the engine scans the joined table for each row, \
                  turning the join into a nested scan."
                .into(),
            remedies: vec![remedy(
                "Index the join key",
                "Index the column used in the join condition.",
                "Create an index on the joined side's key, for example `orders(user_id)`.",
                "The join seeks matching rows via the index instead of scanning.",
                "CREATE INDEX orders_user_id ON orders(user_id)",
            )],
        },

        "parameterized-like-pattern" => Parts {
            title: "Parameterized LIKE may hide a leading wildcard".into(),
            what: "A `LIKE` compares against a parameter, so the pattern is unknown and may start \
                   with `%`."
                .into(),
            why: "If the bound value starts with a wildcard the index cannot be used, and the query \
                  cannot tell. The plan is at the mercy of the input."
                .into(),
            remedies: vec![remedy(
                "Anchor the pattern or validate the input",
                "Ensure the parameter cannot start with a wildcard, or use full-text search for \
                 substrings.",
                "Bind a prefix pattern (`:prefix || '%'`) when prefix matching is enough, or move \
                 substring search to a full-text index.",
                "A non-wildcard prefix keeps the index usable regardless of input.",
                "WHERE name LIKE :prefix || '%'",
            )],
        },

        "parameterized-offset" => Parts {
            title: "Parameterized OFFSET risks deep pagination".into(),
            what: "Pagination uses `OFFSET :n`, so the offset can grow without bound at runtime."
                .into(),
            why: "A large offset reads and discards that many rows. With the offset parameterized, \
                  deep pages silently get slower as users page further."
                .into(),
            remedies: vec![remedy(
                "Use keyset (seek) pagination",
                "Page by the last row's key instead of a counted offset.",
                "Replace `OFFSET :n` with `WHERE key > :last_key ORDER BY key LIMIT :n`.",
                "Every page costs the same, regardless of depth.",
                "WHERE id > :last_id ORDER BY id LIMIT 20",
            )],
        },

        "offset-without-limit" => Parts {
            title: "OFFSET without LIMIT scans the rest of the table".into(),
            what: "An `OFFSET` has no `LIMIT`, so the query skips rows then returns all the rest."
                .into(),
            why: "The engine reads and discards the offset rows, then returns everything after \
                  them. Usually a `LIMIT` was intended."
                .into(),
            remedies: vec![remedy(
                "Add a LIMIT, or use keyset pagination",
                "Bound the page size, or page by key.",
                "Add `LIMIT :n` after the `OFFSET`, or switch to keyset pagination.",
                "The query returns a bounded page instead of the whole tail.",
                "... ORDER BY id LIMIT 20 OFFSET :n",
            )],
        },

        "deferred-join-pagination" => Parts {
            title: "Paginated query could defer its joins".into(),
            what: "A paginated query joins extra tables before applying `LIMIT`, so the join runs \
                   for rows that are then discarded."
                .into(),
            why: "The join materializes columns for every candidate row, but only a page survives \
                  the `LIMIT`. Choosing the page first does far less work."
                .into(),
            remedies: vec![remedy(
                "Paginate the keys first, then join",
                "Select and limit the primary keys, then join the detail tables to that page.",
                "Wrap the paginated `id` selection in a subquery and join the detail tables to it.",
                "The join runs for one page of rows instead of the whole candidate set.",
                "JOIN (SELECT id FROM t ORDER BY id LIMIT 20 OFFSET :n) p ON p.id = t.id",
            )],
        },

        "excessive-joins" => Parts {
            title: "Query joins many tables".into(),
            what: "The query joins more tables than a planner can reliably order, raising plan risk."
                .into(),
            why: "Join planning is combinatorial. Past a handful of tables the optimizer may pick a \
                  poor order, and the query becomes fragile and hard to reason about."
                .into(),
            remedies: vec![remedy(
                "Split the query, or denormalize",
                "Break the query into steps, or pre-join the hot tables.",
                "Materialize an intermediate result (a CTE or temp table), or denormalize the \
                 most-joined columns.",
                "Fewer joins per statement give the planner a tractable problem and a stabler plan.",
                "WITH base AS (SELECT ...) SELECT ... FROM base JOIN ...",
            )],
        },

        "implicit-cast-in-filter" => Parts {
            title: "Implicit cast on the column defeats the index".into(),
            what: "A column is compared to a value of a different type, so the engine casts the \
                   column and cannot use its index."
                .into(),
            why: "Casting the column changes its representation per row, which a B-tree on the raw \
                  column cannot match. The planner scans."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Compare using the column's type",
                "Make the literal match the column's type so the column stays bare.",
                "Quote or cast the literal, not the column, for example `id = '5'` becomes \
                 `id = 5`.",
                "A bare indexed column with a matching literal can seek the index.",
                "WHERE id = 5",
            )],
        },

        "union-vs-union-all" => Parts {
            title: "UNION dedups when UNION ALL may be intended".into(),
            what: "The query combines result sets with `UNION`, which removes duplicate rows.".into(),
            why: "`UNION` sorts or hashes the whole result to drop duplicates. If the branches \
                  cannot overlap, that work is wasted and `UNION ALL` is faster."
                .into(),
            remedies: vec![remedy(
                "Use UNION ALL when duplicates are impossible or fine",
                "Skip the dedup pass unless you actually need distinct rows.",
                "Replace `UNION` with `UNION ALL` when the branches cannot produce the same row.",
                "`UNION ALL` concatenates without the sort/hash dedup, so it is cheaper.",
                "SELECT ... UNION ALL SELECT ...",
            )],
        },

        "complex-subquery-in-where" => Parts {
            title: "Correlated subquery in WHERE could be a join".into(),
            what: "A `WHERE` uses a correlated subquery (one that references the outer row) where a \
                   join would do."
                .into(),
            why: "A correlated subquery may be evaluated once per outer row. Expressed as a join, \
                  the planner can choose a hash or merge strategy and run it once."
                .into(),
            remedies: vec![remedy(
                "Rewrite the subquery as a join",
                "Turn the correlated `IN`/`EXISTS` into an explicit join.",
                "Replace `WHERE a.id IN (SELECT b.aid FROM b WHERE ...)` with \
                 `JOIN b ON b.aid = a.id WHERE ...`.",
                "A join lets the planner pick an efficient strategy instead of a per-row subquery.",
                "FROM a JOIN b ON b.aid = a.id",
            )],
        },

        "correlated-subquery" => Parts {
            title: "Correlated subquery runs per outer row".into(),
            what: "A subquery references a column from the outer query, so it may run once for \
                   every outer row."
                .into(),
            why: "Per-row execution turns one query into N queries. A join or a derived table \
                  usually does the same work in a single pass."
                .into(),
            remedies: vec![remedy(
                "Rewrite as a join or a derived table",
                "Compute the inner result once and join to it.",
                "Move the correlated subquery into the `FROM` as a join or a grouped derived table.",
                "The inner work runs once instead of once per outer row.",
                "FROM a JOIN (SELECT aid, ... FROM b GROUP BY aid) b ON b.aid = a.id",
            )],
        },

        "scalar-subquery-in-select" => Parts {
            title: "Scalar subquery in SELECT runs per row".into(),
            what: "The `SELECT` list contains a correlated scalar subquery, evaluated for each \
                   returned row."
                .into(),
            why: "Each output row triggers the subquery again, often an N+1 pattern. A join \
                  computes the same value in one pass."
                .into(),
            remedies: vec![remedy(
                "Join instead of a per-row subquery",
                "Fetch the value with a join or a grouped derived table.",
                "Replace `SELECT a.*, (SELECT count(*) FROM b WHERE b.aid = a.id)` with a \
                 `LEFT JOIN` to a grouped subquery.",
                "The value is computed once per group instead of once per row.",
                "LEFT JOIN (SELECT aid, count(*) c FROM b GROUP BY aid) b ON b.aid = a.id",
            )],
        },

        "repeated-filter-join" => Parts {
            title: "Same filtered join repeated".into(),
            what: "The query joins the same table with the same filter more than once.".into(),
            why: "Each repetition re-scans and re-filters the same rows. Joining once and reusing \
                  the result avoids the duplicate work."
                .into(),
            remedies: vec![remedy(
                "Join once and reuse",
                "Factor the repeated join into a single CTE or derived table.",
                "Move the repeated filtered join into a `WITH` clause and reference it where \
                 needed.",
                "The filtered set is built once instead of per use.",
                "WITH active AS (SELECT ... FROM b WHERE b.status = 'active') SELECT ... \
                 FROM a JOIN active ON ...",
            )],
        },

        "repeated-scalar-subquery" => Parts {
            title: "Same scalar subquery repeated".into(),
            what: "The same correlated scalar subquery appears more than once in the query.".into(),
            why: "Each copy is evaluated independently, multiplying the per-row cost. Computing it \
                  once and reusing it removes the duplication."
                .into(),
            remedies: vec![remedy(
                "Compute once via a join or CTE",
                "Evaluate the subquery a single time and reference the result.",
                "Join to a derived table that computes the value, then reference its column \
                 wherever the subquery was used.",
                "The value is computed once instead of once per occurrence per row.",
                "LEFT JOIN (SELECT aid, max(x) m FROM b GROUP BY aid) b ON b.aid = a.id",
            )],
        },

        "top-n-per-group" => Parts {
            title: "Top-N per group via a correlated subquery".into(),
            what: "The query picks the top row per group with a correlated `ORDER BY ... LIMIT 1` \
                   (or a per-group max subquery)."
                .into(),
            why: "The inner ordering runs once per group, slow for many groups. A window function \
                  or a LATERAL join does it in one pass."
                .into(),
            remedies: vec![remedy(
                "Use a window function or LATERAL",
                "Rank rows per group once, or use a LATERAL top-N join.",
                "Use `ROW_NUMBER() OVER (PARTITION BY grp ORDER BY x DESC)` and keep rank 1, or a \
                 `LATERAL (... ORDER BY x LIMIT 1)` join.",
                "The ranking is computed in a single pass instead of a subquery per group.",
                "ROW_NUMBER() OVER (PARTITION BY grp ORDER BY x DESC)",
            )],
        },

        "filter-only-join" => Parts {
            title: "Join used only to test existence".into(),
            what: "A table is joined only to check that a matching row exists, not to return its \
                   columns."
                .into(),
            why: "A plain join can multiply rows (one per match) and forces the planner to read the \
                  joined columns. `EXISTS` short-circuits at the first match."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Rewrite as EXISTS",
                "Use `EXISTS` when you only need to know a match exists.",
                "Replace the existence-only join with \
                 `WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.id)`.",
                "`EXISTS` stops at the first match and never duplicates rows.",
                "WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.id)",
            )],
        },

        "count-star-vs-exists" => Parts {
            title: "COUNT(*) used only to test existence".into(),
            what: "The query computes `COUNT(*)` only to check whether any row exists (`count > 0`)."
                .into(),
            why: "`COUNT(*)` reads every matching row to total them. `EXISTS` stops at the first, so \
                  it is far cheaper when you only need yes or no."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Use EXISTS instead of COUNT",
                "Test existence directly.",
                "Replace `(SELECT count(*) FROM b WHERE ...) > 0` with \
                 `EXISTS (SELECT 1 FROM b WHERE ...)`.",
                "`EXISTS` returns at the first match instead of counting all of them.",
                "WHERE EXISTS (SELECT 1 FROM b WHERE b.aid = a.id)",
            )],
        },

        "redundant-distinct-in-subquery" => Parts {
            title: "DISTINCT inside an IN/EXISTS subquery is redundant".into(),
            what: "A subquery feeding `IN` or `EXISTS` uses `DISTINCT`, which has no effect there."
                .into(),
            why: "`IN`/`EXISTS` already test membership, so deduplicating the subquery first is \
                  wasted sort or hash work."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the DISTINCT",
                "Remove `DISTINCT` from the membership subquery.",
                "Delete `DISTINCT` from the `IN (SELECT DISTINCT ...)` subquery.",
                "`IN`/`EXISTS` ignore duplicates, so the dedup pass is pure overhead.",
                "WHERE a.id IN (SELECT b.aid FROM b)",
            )],
        },

        "or-in-join-on" => Parts {
            title: "OR in the JOIN ON can block index use on the join".into(),
            what: "A join `ON` condition contains `OR`, so the join key is no longer a single \
                   equality the planner can look up through an index."
                .into(),
            why: "The planner can only serve a join with an index (or a hash/merge join) when the \
                  condition is a plain equality. An `OR` rules that out, so it falls back to a \
                  nested loop that scans the whole other table for every row — O(n×m), quadratic \
                  as the tables grow. Losing the index is the real cost; the lost hash join is \
                  secondary, since indexed nested loops are the common, fast case."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Split into a UNION of equality joins",
                "Run one equality join per OR arm and union the results.",
                "Rewrite the `OR`-ed join as two joins, each on one equality, combined with \
                 `UNION`.",
                "Each branch is a single equality the planner can serve with an index (and hash \
                 or merge).",
                "SELECT ... JOIN b ON b.x = a.x\nUNION\nSELECT ... JOIN b ON b.y = a.y",
            )],
        },

        "conditional-in-join-on" => Parts {
            title: "CASE in JOIN ON can block index use on the join".into(),
            what: "A join `ON` wraps the join column in a `CASE` (or other conditional) instead of \
                   comparing it directly."
                .into(),
            why: "An index can only be used on the bare column, not on a `CASE` over it. Wrapping \
                  the join key defeats the index, so the join scans the whole other table for \
                  every row — O(n×m), quadratic as the tables grow. Losing the index is the real \
                  cost; the lost hash/merge join is secondary."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Make the join key a bare column",
                "Move the condition out of the join key, or split into branches.",
                "Lift the `CASE` into a `WHERE` or `UNION`, leaving the `ON` a plain `a.x = b.x`.",
                "A bare-column key lets the planner use the index (and hash or merge the join).",
                "JOIN b ON b.x = a.x",
            )],
        },

        "subquery-in-join-on" => Parts {
            title: "Subquery in JOIN ON runs per row".into(),
            what: "A join `ON` contains a subquery, evaluated for each row pair the join considers."
                .into(),
            why: "A subquery in the join condition cannot be a hash key and may run per row, so the \
                  join degrades to a nested loop with a subquery inside."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Precompute the subquery, then join on a column",
                "Materialize the subquery result and join on its value.",
                "Move the subquery into a derived table in `FROM`, then join on its column with a \
                 plain equality.",
                "The subquery runs once and the join becomes a simple equality.",
                "JOIN (SELECT id, ... FROM b) b ON b.id = a.b_id",
            )],
        },

        "distinct-on-grouped" => Parts {
            title: "DISTINCT over a GROUP BY is redundant".into(),
            what: "The query uses `DISTINCT` on top of a `GROUP BY`, which already produces \
                   distinct groups."
                .into(),
            why: "`GROUP BY` collapses each group to one row, so a following `DISTINCT` \
                  re-deduplicates an already-unique result, paying for an extra sort or hash."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the DISTINCT",
                "Remove `DISTINCT` when the query already groups.",
                "Delete `DISTINCT` from a `SELECT DISTINCT ... GROUP BY ...` query.",
                "`GROUP BY` guarantees distinct groups, so the dedup pass is wasted.",
                "SELECT a, b FROM t GROUP BY a, b",
            )],
        },

        "in-list-of-one" => Parts {
            title: "IN with a single value is just =".into(),
            what: "An `IN` list contains exactly one value.".into(),
            why: "`IN (x)` is equivalent to `= x` but reads as a set test, which is slightly \
                  misleading and occasionally plans differently."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Replace IN with =",
                "Use `=` for a single value.",
                "Rewrite `status IN ('open')` as `status = 'open'`.",
                "`=` states the single-value comparison plainly.",
                "WHERE status = 'open'",
            )],
        },

        "positional-reference" => Parts {
            title: "ORDER BY / GROUP BY by column position".into(),
            what: "An `ORDER BY` or `GROUP BY` references a column by its position number, such as \
                   `ORDER BY 1`."
                .into(),
            why: "A positional reference silently points at a different column if the `SELECT` list \
                  is reordered or edited, a fragile coupling that is easy to break."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Use the column name",
                "Reference the column by name, not its position.",
                "Replace `ORDER BY 1` with the column's name.",
                "Naming the column survives changes to the SELECT list.",
                "ORDER BY created_at",
            )],
        },

        "order-by-in-subquery-without-limit" => Parts {
            title: "ORDER BY in a subquery without LIMIT is wasted".into(),
            what: "A subquery (or derived table) has an `ORDER BY` but no `LIMIT`, so the sort does \
                   nothing useful."
                .into(),
            why: "SQL does not guarantee a subquery's order carries to the outer query, and with no \
                  `LIMIT` there is nothing to select with that order. The sort is pure cost."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the ORDER BY, or move it to the outer query",
                "Sort where it matters, at the outermost level.",
                "Remove the `ORDER BY` from the subquery, or add a `LIMIT` if it feeds a top-N.",
                "The outer query controls the final order, so the inner sort is removed.",
                "SELECT ... FROM (SELECT ... FROM t) s ORDER BY s.x",
            )],
        },

        "constant-predicate" => Parts {
            title: "Predicate with only literal operands".into(),
            what: "A comparison has constant operands on both sides, such as `1 = 1`, so it is \
                   always true or always false."
                .into(),
            why: "A constant predicate filters nothing (or everything) and usually means a \
                  placeholder was left in or a column reference was dropped."
                .into(),
            remedies: vec![remedy(
                "Remove or fix the condition",
                "Delete the constant test, or restore the intended column.",
                "Drop a leftover `1 = 1`, or replace the constant with the column you meant to \
                 compare.",
                "The `WHERE` then expresses a real filter.",
                "WHERE status = 'open'",
            )],
        },

        "inconsistent-parameter-type" => Parts {
            title: "Parameter compared against mismatched types".into(),
            what: "The same parameter is compared to columns of different types in one query, so \
                   its bind type is ambiguous."
                .into(),
            why: "The driver must pick one type for the parameter, and the other comparison then \
                  needs an implicit cast that can defeat an index or change results."
                .into(),
            remedies: vec![remedy(
                "Use one type per parameter",
                "Bind a parameter against a single column type, or use separate parameters.",
                "Compare the parameter to one type, or introduce a second parameter for the other \
                 column.",
                "Each parameter has an unambiguous type and each comparison can use its index.",
                "WHERE a.id = :id AND b.code = :code",
            )],
        },

        "pipe-operator-portability" => Parts {
            title: "|| means different things across engines".into(),
            what: "The query uses `||`, which is string concatenation in Postgres, SQLite, and the \
                   SQL standard, but logical OR in MySQL (without `PIPES_AS_CONCAT`)."
                .into(),
            why: "The same query gives different results on different engines, a portability trap \
                  if the SQL ever moves to MySQL."
                .into(),
            remedies: vec![remedy(
                "Be explicit: concat() or OR",
                "Say what you mean rather than relying on `||`.",
                "Use `concat(a, b)` for string concatenation, or `OR` for boolean logic.",
                "The intent is unambiguous on every engine.",
                "SELECT concat(first_name, last_name)",
            )],
        },

        "parse-error" => Parts {
            title: "Query does not parse".into(),
            what: f.message.clone(),
            why: "The statement is not valid SQL as written, so nothing else can be analyzed until \
                  it parses."
                .into(),
            remedies: Vec::new(),
        },

        "schema-ignored" => Parts {
            title: "Schema could not be used".into(),
            what: f.message.clone(),
            why: "The provided schema did not parse or did not apply, so schema-aware checks \
                  (unknown columns, index advice) were skipped."
                .into(),
            remedies: vec![remedy(
                "Check the schema DDL",
                "Make sure the schema is valid `CREATE TABLE` / `CREATE INDEX` for the chosen \
                 dialect.",
                "Fix the DDL so it parses under the same dialect as the query.",
                "Valid schema enables column resolution and index advice.",
                "CREATE TABLE users (id int PRIMARY KEY, email text)",
            )],
        },

        "redundant-distinct-in-union" => Parts {
            title: "DISTINCT inside a UNION is redundant".into(),
            what: "A branch of a plain `UNION` uses `SELECT DISTINCT`.".into(),
            why: "`UNION` already removes duplicates from the combined result, so a `DISTINCT` \
                  inside a branch only forces a second, wasted dedup pass. (A `UNION ALL` branch is \
                  different — it keeps duplicates.)"
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the branch DISTINCT",
                "The UNION deduplicates for you.",
                "Remove `DISTINCT` from the branch; the `UNION` still returns distinct rows.",
                "One dedup pass instead of two, with the same result.",
                "SELECT a FROM t UNION SELECT a FROM u",
            )],
        },

        "count-nullable-column" => Parts {
            title: "COUNT(col) skips NULLs".into(),
            what: "`COUNT(col)` is used on a column the schema allows to be NULL.".into(),
            why: "`COUNT(col)` counts only rows where the column is non-NULL, so on a nullable \
                  column it returns fewer than the row count. If you meant \"how many rows\", that \
                  is `COUNT(*)`."
                .into(),
            remedies: vec![remedy(
                "Use COUNT(*) to count rows",
                "Count every row regardless of NULLs.",
                "Replace `COUNT(col)` with `COUNT(*)`, unless you specifically want the non-NULL \
                 count.",
                "`COUNT(*)` counts all rows; keep `COUNT(col)` only when the non-NULL count is the \
                 intent.",
                "SELECT COUNT(*) FROM t",
            )],
        },

        "redundant-else-null" => Parts {
            title: "ELSE NULL is redundant".into(),
            what: "A `CASE` ends with `ELSE NULL`.".into(),
            why: "When no `WHEN` matches, a `CASE` already returns NULL, so an explicit `ELSE NULL` \
                  changes nothing."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the ELSE NULL",
                "The implicit else is already NULL.",
                "Remove the trailing `ELSE NULL`; the `CASE` behaves identically.",
                "Shorter expression, same result.",
                "CASE WHEN a > 0 THEN 1 END",
            )],
        },

        "case-to-coalesce" => Parts {
            title: "This CASE is a hand-rolled COALESCE".into(),
            what: "A `CASE` returns the first non-NULL of two columns, e.g. \
                   `CASE WHEN x IS NULL THEN y ELSE x END`."
                .into(),
            why: "That is exactly what `COALESCE(x, y)` does — one standard expression that reads \
                  clearly and works on every dialect."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Use COALESCE",
                "COALESCE returns its first non-NULL argument.",
                "Replace the CASE with `COALESCE(x, y)`.",
                "Shorter, standard, and identical in behavior.",
                "SELECT COALESCE(a, b) FROM t",
            )],
        },

        "columns-in-exists" => Parts {
            title: "EXISTS ignores its projection".into(),
            what: "An `EXISTS` subquery selects specific columns.".into(),
            why: "`EXISTS` only tests whether any row exists; its SELECT list is never evaluated. \
                  Naming columns implies they matter when they do not."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Select 1 instead",
                "State that only existence matters.",
                "Replace the column list with `SELECT 1`.",
                "`SELECT 1` reads as an existence test and the plan is unchanged.",
                "WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = u.id)",
            )],
        },

        "redundant-cast" => Parts {
            title: "Cast to the type the column already has".into(),
            what: "A column is cast to its own type, e.g. `CAST(created AS date)` on a `date` \
                   column."
                .into(),
            why: "The cast is a no-op that only clutters the expression.".into(),
            remedies: vec![remedy(
                "Drop the cast",
                "Use the column directly.",
                "Remove the cast: `created` instead of `CAST(created AS date)`.",
                "Same value, less noise.",
                "SELECT created FROM t",
            )],
        },

        "redundant-group-by-on-key" => Parts {
            title: "GROUP BY covers a primary key".into(),
            what: "The grouping keys include a table's whole primary key.".into(),
            why: "The primary key already makes each row unique, so every group holds exactly one \
                  row and the aggregation collapses nothing. Either the `GROUP BY` is redundant, or \
                  a coarser key was intended."
                .into(),
            remedies: vec![remedy(
                "Group by a coarser key, or drop the GROUP BY",
                "Decide what the query should aggregate over.",
                "Remove the `GROUP BY` for per-row output, or group by the column you meant to \
                 collapse on.",
                "The query then aggregates real groups instead of single rows.",
                "SELECT dept, SUM(x) FROM t GROUP BY dept",
            )],
        },

        "is-null-on-not-null-column" => Parts {
            title: "IS NULL on a NOT NULL column is always false".into(),
            what: "A `col IS NULL` predicate tests a column the schema declares NOT NULL.".into(),
            why: "A NOT NULL column can never be NULL, so the predicate is always false and the \
                  query silently returns no rows — usually a bug (a wrong column or a stale \
                  filter). Outer joins are the exception: there a NOT NULL column can be NULL on \
                  the null-extended side (the anti-join idiom)."
                .into(),
            remedies: vec![remedy(
                "Check the column",
                "As written, this predicate matches nothing.",
                "Confirm you meant this column; for an anti-join the column must come from the \
                 null-extended side of an outer join.",
                "Fixing the column makes the filter select the rows you intended.",
                "WHERE deleted_at IS NULL   -- on a nullable column",
            )],
        },

        "not-in-list-with-null" => Parts {
            title: "NOT IN a list containing NULL matches nothing".into(),
            what: "A `NOT IN (…)` list has a `NULL` in it.".into(),
            why: "`x NOT IN (a, b, NULL)` is `x <> a AND x <> b AND x <> NULL`, and `x <> NULL` is \
                  never true — so the whole predicate is never true and the query silently returns \
                  zero rows."
                .into(),
            remedies: vec![remedy(
                "Remove the NULL, or use NOT EXISTS",
                "The NULL makes the predicate unsatisfiable.",
                "Drop the `NULL` from the list, or rewrite the check as `NOT EXISTS`.",
                "Without the NULL the comparison behaves as intended.",
                "WHERE x NOT IN (1, 2)",
            )],
        },

        "group-by-constant" => Parts {
            title: "GROUP BY a constant".into(),
            what: "A `GROUP BY` key is a constant (`'x'`, `NULL`, a literal number).".into(),
            why: "A constant is the same for every row. Postgres and SQL Server reject it (a hard \
                  error); MySQL and SQLite collapse the whole table into a single group — rarely \
                  the intent (often a forgotten column, or a stray literal)."
                .into(),
            remedies: vec![remedy(
                "Group by the intended column",
                "Pick the column you meant to group on.",
                "Replace the constant with the grouping column, or drop `GROUP BY` for a plain \
                 aggregate.",
                "The query then aggregates real groups.",
                "GROUP BY dept",
            )],
        },

        "in-subquery-select-star" => Parts {
            title: "IN (SELECT *) — project one column".into(),
            what: "An `IN` subquery uses `SELECT *`.".into(),
            why: "`IN` compares against exactly one column, so `SELECT *` errors the moment the \
                  subquery table has more than one column, and breaks silently if a column is \
                  added later."
                .into(),
            remedies: vec![remedy(
                "Name the single column",
                "Select just the column the IN compares against.",
                "Replace `IN (SELECT * FROM t)` with `IN (SELECT id FROM t)`.",
                "The subquery returns one stable column regardless of schema changes.",
                "WHERE x IN (SELECT id FROM t)",
            )],
        },

        "sum-case-to-count-filter" => Parts {
            title: "SUM(CASE … 1 ELSE 0) is a conditional count".into(),
            what: "`SUM(CASE WHEN c THEN 1 ELSE 0 END)` sums ones and zeros over a condition.".into(),
            why: "That is just counting the rows where the condition holds, written the long way.".into(),
            remedies: vec![remedy(
                "Use a filtered COUNT",
                "Count the matching rows directly.",
                "`COUNT(*) FILTER (WHERE c)` (Postgres/SQLite), or `SUM(c)` on MySQL where a boolean \
                 is 1/0.",
                "Shorter and states the intent — a count of matching rows.",
                "COUNT(*) FILTER (WHERE active)",
            )],
        },

        "boolean-literal-comparison" => Parts {
            title: "Comparing a boolean column to true/false".into(),
            what: "A boolean column is compared to a boolean literal (`col = true`).".into(),
            why: "A boolean column is already a truth value, so `col = true` is just `col` and \
                  `col = false` is `NOT col`. The literal only adds noise."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Use the column directly",
                "Drop the redundant literal comparison.",
                "Rewrite `col = true` as `col`, and `col = false` as `NOT col`.",
                "Same result, less noise.",
                "WHERE active",
            )],
        },

        "timestamp-compared-to-date" => Parts {
            title: "Timestamp = date matches only midnight".into(),
            what: "A timestamp column is equated to a date-only value (`ts = '2024-01-01'`).".into(),
            why: "The date widens to midnight, so only rows at exactly 00:00:00 match — every later \
                  instant that day is silently excluded."
                .into(),
            remedies: vec![remedy(
                "Match the whole day with a range",
                "Use a half-open range instead of equality.",
                "Rewrite `ts = '2024-01-01'` as `ts >= '2024-01-01' AND ts < '2024-01-02'`.",
                "The range covers every instant of the day.",
                "WHERE ts >= '2024-01-01' AND ts < '2024-01-02'",
            )],
        },

        "count-distinct-on-unique" => Parts {
            title: "COUNT(DISTINCT) on a unique column".into(),
            what: "`COUNT(DISTINCT col)` where `col` is a primary key or `UNIQUE`.".into(),
            why: "The values are already distinct, so `DISTINCT` adds a sort/hash that removes \
                  nothing — `COUNT(col)` returns the same number more cheaply."
                .into(),
            remedies: vec![remedy(
                "Drop the DISTINCT",
                "The column is already unique.",
                "Replace `COUNT(DISTINCT id)` with `COUNT(id)`.",
                "Same count, without the redundant dedup.",
                "COUNT(id)",
            )],
        },

        "redundant-coalesce-on-not-null" => Parts {
            title: "COALESCE on a NOT NULL column".into(),
            what: "`COALESCE(col, …)` whose first argument is a NOT NULL column.".into(),
            why: "The column is never NULL, so `COALESCE` always returns it and the fallback \
                  arguments are unreachable dead code."
                .into(),
            remedies: vec![remedy(
                "Use the column directly",
                "The fallback can never be reached.",
                "Replace `COALESCE(col, x)` with `col`.",
                "Same value, without the dead branch.",
                "SELECT col FROM t",
            )],
        },

        "is-not-null-on-not-null-column" => Parts {
            title: "IS NOT NULL on a NOT NULL column is always true".into(),
            what: "A `col IS NOT NULL` predicate tests a column the schema declares NOT NULL.".into(),
            why: "A NOT NULL column is never NULL, so the check is always true and filters nothing. \
                  (Outer joins are the exception: there the column can be NULL on the null-extended \
                  side, so the check is a real matched-rows filter.)"
                .into(),
            remedies: vec![remedy(
                "Drop the redundant filter",
                "The predicate never excludes a row.",
                "Remove the `IS NOT NULL` check (unless the column comes from the null-extended side \
                 of an outer join).",
                "The query is simpler and behaves identically.",
                "WHERE true   -- i.e. omit the check",
            )],
        },

        "aggregate-in-where" => Parts {
            title: "Aggregate in WHERE — use HAVING".into(),
            what: "An aggregate (`count`, `sum`, …) is used in the `WHERE` clause.".into(),
            why: "`WHERE` filters rows before grouping, so aggregates aren't computed yet — every \
                  engine rejects an aggregate there. The condition belongs in `HAVING`, which \
                  filters after grouping."
                .into(),
            remedies: vec![remedy(
                "Move the condition to HAVING",
                "Filter on the aggregate after grouping.",
                "Replace `WHERE count(*) > 5` with `GROUP BY … HAVING count(*) > 5`.",
                "`HAVING` runs after the aggregate is computed, so the query is valid.",
                "GROUP BY dept HAVING count(*) > 5",
            )],
        },

        "mixed-aggregate-and-column" => Parts {
            title: "Aggregate mixed with a non-grouped column".into(),
            what: "A SELECT has an aggregate and a bare column, with no `GROUP BY`.".into(),
            why: "Postgres and SQL Server reject this (every non-aggregated column must be \
                  grouped); MySQL and SQLite run it and return the bare column from an arbitrary \
                  row, so the result is nondeterministic."
                .into(),
            remedies: vec![remedy(
                "Group it or aggregate it",
                "Decide how the bare column relates to the aggregate.",
                "Add a `GROUP BY`, or wrap the column in an aggregate such as `MAX(col)`.",
                "The query becomes valid and its result well-defined.",
                "SELECT dept, MAX(x) FROM t GROUP BY dept",
            )],
        },

        "natural-join" => Parts {
            title: "NATURAL JOIN keys on shared column names".into(),
            what: "A `NATURAL JOIN` joins on whatever columns the two tables share by name.".into(),
            why: "The join keys are chosen implicitly and aren't visible at the call site. A new \
                  same-named column silently changes the join condition or breaks the query."
                .into(),
            remedies: vec![remedy(
                "Use an explicit JOIN … ON / USING",
                "State the join keys.",
                "Replace `a NATURAL JOIN b` with `a JOIN b ON a.k = b.k` (or `USING (k)`).",
                "The keys are explicit and stable against schema changes.",
                "FROM a JOIN b USING (id)",
            )],
        },

        "select-distinct-on-unique" => Parts {
            title: "DISTINCT on an already-unique projection".into(),
            what: "`SELECT DISTINCT` where a projected column is a primary key or `UNIQUE`.".into(),
            why: "A unique column already makes every row distinct, so `DISTINCT` removes nothing — \
                  it just adds a sort/hash."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the DISTINCT",
                "The row is already unique.",
                "Remove `DISTINCT`; the result is unchanged.",
                "Saves the redundant dedup pass.",
                "SELECT id FROM t",
            )],
        },

        "between-timestamp-date-bounds" => Parts {
            title: "BETWEEN with date bounds drops the last day".into(),
            what: "A timestamp column is `BETWEEN` two date-only values.".into(),
            why: "The upper bound widens to midnight, so every time on the last day after 00:00:00 \
                  is excluded — rows are silently dropped."
                .into(),
            remedies: vec![remedy(
                "Use a half-open range",
                "Include the whole final day.",
                "Rewrite `ts BETWEEN low AND high` as `ts >= low AND ts < high_plus_one_day`.",
                "The range then covers every instant up to (not including) the next day.",
                "WHERE ts >= '2024-01-01' AND ts < '2024-02-01'",
            )],
        },

        "redundant-subquery-in-from" => Parts {
            title: "Derived table just wraps a base table".into(),
            what: "A `FROM` subquery is exactly `SELECT * FROM t` with no clauses of its own.".into(),
            why: "The wrapper filters, groups, and projects nothing — it only adds a layer of \
                  indirection over the base table."
                .into(),
            remedies: vec![remedy(
                "Flatten it",
                "Select from the base table directly.",
                "Replace `FROM (SELECT * FROM t) x` with `FROM t x`.",
                "Same result, one less level.",
                "FROM t x",
            )],
        },

        "double-negation" => Parts {
            title: "Negated comparison reads backwards".into(),
            what: "A negated comparison (`NOT (a <> b)`) or a doubled negation (`NOT NOT x`).".into(),
            why: "Negating a comparison or a negation is harder to read than the equivalent \
                  positive form."
                .into(),
            remedies: vec![remedy(
                "State the positive form",
                "Cancel the negation.",
                "Rewrite `NOT (a <> b)` as `a = b`, and `NOT NOT x` as `x`.",
                "The predicate reads directly.",
                "WHERE a = b",
            )],
        },

        "right-join" => Parts {
            title: "RIGHT JOIN reads backwards".into(),
            what: "The query uses a `RIGHT JOIN`.".into(),
            why: "A `RIGHT JOIN` keeps the right-hand table's rows, inverting the usual \
                  left-to-right reading. The equivalent `LEFT JOIN` keeps the driving table first."
                .into(),
            remedies: vec![remedy(
                "Swap the tables and use LEFT JOIN",
                "Put the kept table on the left.",
                "Rewrite `a RIGHT JOIN b` as `b LEFT JOIN a`.",
                "The join reads in the conventional direction.",
                "FROM b LEFT JOIN a ON a.id = b.id",
            )],
        },

        "redundant-nullif" => Parts {
            title: "NULLIF with identical arguments is always NULL".into(),
            what: "`NULLIF(x, x)` — the same expression on both sides.".into(),
            why: "`NULLIF(a, b)` returns NULL when `a = b`, so with identical arguments it is always \
                  NULL — never useful, usually a typo for a different second argument."
                .into(),
            remedies: vec![remedy(
                "Check the second argument",
                "It should be the sentinel value to map to NULL.",
                "Set the second argument to the value you want to turn into NULL, e.g. \
                 `NULLIF(x, 0)`.",
                "`NULLIF` then maps that value to NULL and passes everything else through.",
                "NULLIF(x, 0)",
            )],
        },

        "implicit-boolean-int" => Parts {
            title: "Integer column used as a WHERE condition".into(),
            what: "A bare integer column is used directly as a `WHERE` condition (`WHERE active`).".into(),
            why: "A `WHERE` condition must be boolean. Postgres and SQL Server reject a bare integer \
                  (\"argument of WHERE must be boolean\"); MySQL and SQLite treat any nonzero value \
                  as true, so the same query runs there — a portability trap."
                .into(),
            remedies: vec![remedy(
                "Compare explicitly",
                "Turn the integer into a boolean test.",
                "Write `WHERE col <> 0`, or store the flag as a boolean column.",
                "The condition is explicitly boolean and behaves the same on every engine.",
                "WHERE active <> 0",
            )],
        },

        "limit-zero" => Parts {
            title: "LIMIT 0 returns no rows".into(),
            what: "The query caps its result at zero rows with `LIMIT 0`.".into(),
            why: "It always returns nothing — rarely intended. Usually a leftover from debugging or \
                  an unfilled template default."
                .into(),
            remedies: vec![remedy(
                "Remove the LIMIT or set a real count",
                "Decide how many rows you want.",
                "Drop `LIMIT 0`, or set the intended limit.",
                "The query returns rows again.",
                "SELECT * FROM t LIMIT 20",
            )],
        },

        "like-all-wildcards" => Parts {
            title: "LIKE '%' matches everything".into(),
            what: "A `LIKE` pattern of only `%` (`col LIKE '%'`).".into(),
            why: "`%` matches every non-NULL value, so the filter does nothing (beyond excluding \
                  NULLs). Usually the pattern parameter wasn't filled in."
                .into(),
            remedies: vec![remedy(
                "Remove it, or use a real pattern",
                "Apply the filter only when there's something to match.",
                "Drop the condition, or give it a concrete pattern; apply it conditionally if the \
                 term is optional.",
                "The query stops scanning for a filter that matches everything.",
                "WHERE name LIKE 'term%'",
            )],
        },

        "order-by-constant" => Parts {
            title: "ORDER BY a constant orders nothing".into(),
            what: "A sort key is a constant (`ORDER BY 'x'`).".into(),
            why: "A constant is the same for every row, so it imposes no ordering. Postgres and SQL \
                  Server reject a non-integer constant sort key; MySQL and SQLite run it as a no-op."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Drop the constant sort key",
                "It doesn't order anything.",
                "Remove the constant `ORDER BY`, or name the real column you meant to sort by.",
                "The query is simpler (or actually sorted, if you name a column).",
                "SELECT id FROM t",
            )],
        },

        "redundant-order-by-in-union-branch" => Parts {
            title: "ORDER BY in a UNION branch is discarded".into(),
            what: "A branch of a set operation has its own `ORDER BY` (`(SELECT … ORDER BY x) UNION …`).".into(),
            why: "The set operation determines the row order of the combined result, so the branch's \
                  `ORDER BY` is ignored (unless it's `LIMIT`-bounded)."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Order the whole result, or drop it",
                "Put one ORDER BY after the set operation.",
                "Move the `ORDER BY` to after the whole `UNION`, or remove it from the branch.",
                "The final result is ordered (or the no-op is gone).",
                "(SELECT a FROM t) UNION SELECT a FROM u ORDER BY a",
            )],
        },

        "nested-coalesce" => Parts {
            title: "Nested COALESCE — flatten it".into(),
            what: "A `COALESCE` whose argument is itself a `COALESCE`.".into(),
            why: "`COALESCE` takes any number of arguments, so `COALESCE(COALESCE(a, b), c)` is just \
                  `COALESCE(a, b, c)`."
                .into(),
            remedies: vec![remedy(
                "Use one COALESCE",
                "List all the fallbacks in a single call.",
                "Rewrite `COALESCE(COALESCE(a, b), c)` as `COALESCE(a, b, c)`.",
                "Same result, read at a glance.",
                "COALESCE(a, b, c)",
            )],
        },

        "case-when-boolean" => Parts {
            title: "CASE that just returns its condition".into(),
            what: "`CASE WHEN c THEN true ELSE false END` — both branches boolean literals.".into(),
            why: "The `CASE` restates the boolean condition `c`. (It also maps a NULL condition to \
                  `false`, so a projection needs `COALESCE(c, false)` to match exactly.)"
                .into(),
            remedies: vec![remedy(
                "Use the condition directly",
                "The condition is already a boolean.",
                "In a `WHERE`, use `c`; in a projection, `COALESCE(c, false)` to keep the \
                 NULL-to-false behaviour.",
                "Shorter and clearer.",
                "WHERE c",
            )],
        },

        "coalesce-single-arg" => Parts {
            title: "COALESCE with one argument does nothing".into(),
            what: "`COALESCE(x)` — a single argument.".into(),
            why: "`COALESCE` returns the first non-NULL of its arguments, so with one argument it is \
                  just `x`."
                .into(),
            remedies: vec![remedy(
                "Use the argument directly",
                "There's no fallback to choose.",
                "Replace `COALESCE(x)` with `x`, or add the fallback that was meant to be there.",
                "The call no longer hides a plain column.",
                "SELECT x FROM t",
            )],
        },

        "in-list-with-duplicates" => Parts {
            title: "Duplicate value in an IN list".into(),
            what: "An `IN` list repeats a value (`x IN (1, 2, 2)`).".into(),
            why: "`IN` tests set membership, so a repeated value changes nothing — usually a \
                  copy-paste slip or an undeduped generated list."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Remove the duplicate",
                "Each value only needs to appear once.",
                "Delete the repeated value from the list.",
                "Same result, shorter list.",
                "WHERE x IN (1, 2)",
            )],
        },

        "duplicate-column-in-select" => Parts {
            title: "The same column is selected twice".into(),
            what: "The SELECT list projects the same column more than once.".into(),
            why: "It ships a redundant copy and produces two output columns with the same name, \
                  which confuses clients that key results by column name."
                .into(),
            remedies: vec![remedy(
                "Remove the duplicate",
                "Or alias it if you really want it twice.",
                "Delete the repeated column from the SELECT list, or give one an alias.",
                "The result has one column per value.",
                "SELECT id, name FROM t",
            )],
        },

        "duplicate-group-by-key" => Parts {
            title: "The same key appears twice in GROUP BY".into(),
            what: "A column is listed more than once in `GROUP BY` (`GROUP BY a, a`).".into(),
            why: "Grouping by a column twice is identical to grouping by it once, so the repeat is \
                  pure redundancy."
                .into(),
            remedies: vec![apply_remedy(
                f,
                "Remove the duplicate key",
                "It doesn't change the grouping.",
                "Delete the repeated column from the `GROUP BY`.",
                "Same groups, cleaner clause.",
                "GROUP BY a",
            )],
        },

        "exists-with-aggregate" => Parts {
            title: "EXISTS on an aggregate subquery is a constant".into(),
            what: "An `EXISTS` / `NOT EXISTS` wraps a subquery with an unqualified aggregate \
                   (`SELECT count(*) …`) and no `GROUP BY`."
                .into(),
            why: "Such a subquery always returns exactly one row — even over an empty table \
                  (`count(*)` is 0) — so `EXISTS` is always true and `NOT EXISTS` always false. \
                  The test reads like a filter but never changes the result."
                .into(),
            remedies: vec![remedy(
                "Test for rows, not an aggregate",
                "Drop the aggregate and probe for a matching row.",
                "Replace `EXISTS (SELECT count(*) … WHERE …)` with `EXISTS (SELECT 1 … WHERE …)`.",
                "`EXISTS` now stops at the first matching row and actually reflects whether any exist.",
                "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
            )],
        },

        "distinct-on-arbitrary-row" => Parts {
            title: "DISTINCT ON without ORDER BY picks an arbitrary row".into(),
            what: "A Postgres `DISTINCT ON (…)` query has no `ORDER BY`.".into(),
            why: "`DISTINCT ON` keeps one row per group, but with no `ORDER BY` Postgres may pick \
                  any row in the group — the result is nondeterministic and can change between \
                  runs, versions, or plans."
                .into(),
            remedies: vec![remedy(
                "Order so the surviving row is defined",
                "Lead the ORDER BY with the DISTINCT ON expressions, then a tiebreaker.",
                "Add `ORDER BY <distinct-on cols>, <tiebreaker>` (Postgres requires the leading keys to match).",
                "The kept row per group is now well-defined and stable.",
                "SELECT DISTINCT ON (user_id) * FROM events ORDER BY user_id, created_at DESC",
            )],
        },

        "nested-aggregate" => Parts {
            title: "Aggregates cannot be nested".into(),
            what: "An aggregate call sits directly inside another aggregate (`SUM(AVG(x))`).".into(),
            why: "SQL forbids one aggregate inside another; every engine rejects it, so the query \
                  won't run as written."
                .into(),
            remedies: vec![remedy(
                "Aggregate in two steps",
                "Compute the inner aggregate in a subquery, then aggregate that.",
                "Move the inner aggregate into a subquery/CTE and aggregate its result outside.",
                "Each aggregation happens at its own level, which is legal SQL.",
                "SELECT AVG(s) FROM (SELECT SUM(x) AS s FROM t GROUP BY g) q",
            )],
        },

        "plus-string-concat" => Parts {
            title: "`+` is not a portable way to concatenate strings".into(),
            what: "A `+` operator has a string literal as an operand (`'x' + col`).".into(),
            why: "`+` concatenates strings only in SQL Server. Postgres rejects `text + text`; \
                  MySQL and SQLite coerce the string to a number (so `'a' + 'b'` becomes 0), \
                  silently returning the wrong value."
                .into(),
            remedies: vec![remedy(
                "Use `||` or `CONCAT(...)`",
                "The standard concatenation operator, or `CONCAT` for full portability.",
                "Replace `a + b` with `a || b`, or `CONCAT(a, b)` if you must also target SQL Server.",
                "Concatenation is explicit and behaves the same on every engine.",
                "SELECT first_name || ' ' || last_name FROM users",
            )],
        },

        "insert-without-column-list" => Parts {
            title: "INSERT without a column list binds by position".into(),
            what: "An `INSERT … VALUES` (or `INSERT … SELECT`) names no target columns.".into(),
            why: "Values are matched to columns by position, so the statement silently depends on \
                  the table's exact column order — adding, dropping, or reordering a column \
                  misroutes every value or starts erroring."
                .into(),
            remedies: vec![remedy(
                "Name the target columns",
                "List the columns the values map to.",
                "Write `INSERT INTO t (a, b) VALUES (…)` instead of `INSERT INTO t VALUES (…)`.",
                "The insert stays correct across schema changes and reads clearly.",
                "INSERT INTO t (a, b) VALUES (1, 2)",
            )],
        },

        "count-distinct-multiple-columns" => Parts {
            title: "COUNT(DISTINCT a, b) is a MySQL-only extension".into(),
            what: "A `COUNT(DISTINCT …)` takes more than one column argument.".into(),
            why: "Counting distinct combinations of several columns in one `COUNT(DISTINCT …)` \
                  works only in MySQL; Postgres, SQLite, and SQL Server reject the multi-argument \
                  form with a syntax error."
                .into(),
            remedies: vec![remedy(
                "Count over a DISTINCT subquery",
                "De-duplicate the combinations first, then count.",
                "Replace `COUNT(DISTINCT a, b)` with a count over `SELECT DISTINCT a, b`.",
                "Counts distinct combinations portably on every engine.",
                "SELECT count(*) FROM (SELECT DISTINCT a, b FROM t) q",
            )],
        },

        "group-by-without-aggregate" => Parts {
            title: "GROUP BY with no aggregate is just DISTINCT".into(),
            what: "A query has a `GROUP BY` but no aggregate in the SELECT list or `HAVING`.".into(),
            why: "With nothing being aggregated, `GROUP BY` only collapses duplicate rows — exactly \
                  what `SELECT DISTINCT` expresses more directly — or it's a sign an aggregate was \
                  dropped by mistake."
                .into(),
            remedies: vec![remedy(
                "Use DISTINCT, or add the aggregate",
                "Say what you mean: dedup, or aggregate per group.",
                "Switch to `SELECT DISTINCT …`, or add the `count`/`sum`/etc. you intended.",
                "The query's intent is explicit instead of implied by an empty GROUP BY.",
                "SELECT DISTINCT a FROM t",
            )],
        },

        "distinct-star" => Parts {
            title: "SELECT DISTINCT * usually hides a fan-out join".into(),
            what: "A query de-duplicates on every column with `SELECT DISTINCT *`.".into(),
            why: "Deduping the whole row is rarely a real need — usually a join multiplies rows and \
                  `DISTINCT` papers over it, hiding the cardinality bug and adding a full-width \
                  sort/hash."
                .into(),
            remedies: vec![remedy(
                "Fix the join, don't mask it",
                "Find the join that fans out and constrain or aggregate it.",
                "Remove `DISTINCT` and correct the join (add the missing condition, or aggregate).",
                "The real duplication bug is fixed and the costly dedup pass disappears.",
                "SELECT o.* FROM orders o JOIN customers c ON c.id = o.customer_id",
            )],
        },

        "multiple-count-distinct" => Parts {
            title: "Several COUNT(DISTINCT …) columns force separate sorts".into(),
            what: "One query block has two or more `COUNT(DISTINCT …)` over different columns.".into(),
            why: "Each distinct count needs its own sort or hash and the engine can't combine them, \
                  so the query de-duplicates the same rows once per distinct-count."
                .into(),
            remedies: vec![remedy(
                "Pre-aggregate, or split the counts",
                "Group by each key once, or compute each count over a subquery.",
                "Compute each `COUNT(DISTINCT …)` over a pre-grouped subquery instead of side by side.",
                "The de-duplication work is shared instead of repeated per column.",
                "SELECT (SELECT count(*) FROM (SELECT DISTINCT a FROM t) x), (SELECT count(*) FROM (SELECT DISTINCT b FROM t) y)",
            )],
        },

        "count-constant-arg" => Parts {
            title: "COUNT(<constant>) is identical to COUNT(*)".into(),
            what: "A `COUNT` takes a constant argument (`COUNT(1)`, `COUNT('x')`).".into(),
            why: "A constant is never NULL, so it counts exactly the same rows as `COUNT(*)` — there \
                  is no performance difference; the \"COUNT(1) is faster\" belief is a myth."
                .into(),
            remedies: vec![remedy(
                "Write COUNT(*)",
                "The idiom optimizers recognize and readers expect.",
                "Replace `COUNT(1)` (or any `COUNT(<constant>)`) with `COUNT(*)`.",
                "Same result and speed, stated the standard way.",
                "SELECT count(*) FROM t",
            )],
        },

        "union-column-count-mismatch" => Parts {
            title: "Set operation branches have different column counts".into(),
            what: "The two sides of a `UNION`/`INTERSECT`/`EXCEPT` select a different number of columns.".into(),
            why: "Set operations line up columns by position, so both branches must project the \
                  same number of columns — otherwise the query fails to run."
                .into(),
            remedies: vec![remedy(
                "Match the column counts",
                "Select the same number of columns on each side.",
                "Add or drop a column so both SELECT lists have the same width.",
                "The set operation type-checks and runs.",
                "SELECT a, b FROM t UNION SELECT a, b FROM u",
            )],
        },

        "where-references-select-alias" => Parts {
            title: "WHERE can't reference a SELECT alias".into(),
            what: "A `WHERE` predicate uses a name that is only a SELECT-list alias.".into(),
            why: "`WHERE` is evaluated before the SELECT list, so aliases aren't in scope there — \
                  every engine rejects it."
                .into(),
            remedies: vec![remedy(
                "Repeat the expression, or wrap the query",
                "Filter on the underlying expression, or compute the alias first.",
                "Use the original expression in `WHERE`, or move the alias into a subquery/CTE and filter outside.",
                "The filter resolves against columns that are actually in scope.",
                "SELECT * FROM (SELECT a AS x FROM t) q WHERE q.x > 5",
            )],
        },

        "tautology-null-check" => Parts {
            title: "IS NULL OR IS NOT NULL is always true".into(),
            what: "A predicate ORs `x IS NULL` with `x IS NOT NULL` on the same column.".into(),
            why: "Every value is either NULL or not-NULL, so the condition matches every row and \
                  filters nothing."
                .into(),
            remedies: vec![remedy(
                "Remove it, or fix the intent",
                "The clause is a no-op as written.",
                "Delete the redundant pair, or replace it with the condition you meant.",
                "The query says what it actually filters on.",
                "SELECT * FROM t WHERE x > 0",
            )],
        },

        "contradiction-null-check" => Parts {
            title: "IS NULL AND IS NOT NULL is always false".into(),
            what: "A predicate ANDs `x IS NULL` with `x IS NOT NULL` on the same column.".into(),
            why: "A column can't be both NULL and not-NULL, so the predicate never holds and the \
                  query returns no rows."
                .into(),
            remedies: vec![remedy(
                "Fix the contradictory pair",
                "It usually means two conditions got crossed.",
                "Keep the one you meant, or point the two checks at different columns.",
                "The query can return rows again.",
                "SELECT * FROM t WHERE x IS NOT NULL",
            )],
        },

        "in-subquery-with-limit" => Parts {
            title: "LIMIT inside an IN subquery truncates the candidates".into(),
            what: "An `x IN (SELECT … LIMIT n)` limits the set of values membership is tested against.".into(),
            why: "The `IN` only checks the first `n` rows the subquery returns — and without an \
                  `ORDER BY`, which `n` is arbitrary — so real matches are silently missed."
                .into(),
            remedies: vec![remedy(
                "Drop the LIMIT, or move it out",
                "The candidate set shouldn't be capped.",
                "Remove the `LIMIT` from the subquery, or apply it to the query that needs paging.",
                "`IN` tests against the full set and matches correctly.",
                "SELECT * FROM t WHERE a IN (SELECT b FROM u)",
            )],
        },

        "limit-in-derived-table-without-order" => Parts {
            title: "LIMIT in a derived table without ORDER BY".into(),
            what: "A FROM-clause subquery `(SELECT … LIMIT n)` has no `ORDER BY`.".into(),
            why: "The `LIMIT` keeps an arbitrary `n` rows, so the outer query runs over a \
                  nondeterministic subset that can change between runs and plans."
                .into(),
            remedies: vec![remedy(
                "Order the derived table, or move the LIMIT",
                "Make the kept rows well-defined.",
                "Add an `ORDER BY` (with a unique tiebreaker) inside the derived table, or lift the `LIMIT` out.",
                "The subset is stable and reproducible.",
                "SELECT * FROM (SELECT * FROM t ORDER BY id LIMIT 5) q",
            )],
        },

        "offset-without-order-by" => Parts {
            title: "OFFSET without ORDER BY skips arbitrary rows".into(),
            what: "A query uses `OFFSET n` with no `ORDER BY`.".into(),
            why: "Row order is unspecified without `ORDER BY`, so `OFFSET` discards an arbitrary, \
                  run-to-run-unstable set of rows — paging over it skips or repeats rows."
                .into(),
            remedies: vec![remedy(
                "Add a deterministic ORDER BY",
                "Define the order the offset counts against.",
                "Add `ORDER BY <keys>` including a unique column as a tiebreaker.",
                "Pagination becomes stable across requests.",
                "SELECT * FROM t ORDER BY id OFFSET 20",
            )],
        },

        "exists-with-limit" => Parts {
            title: "LIMIT inside EXISTS is redundant".into(),
            what: "An `EXISTS (SELECT … LIMIT n)` subquery carries a `LIMIT`.".into(),
            why: "`EXISTS` stops at the first row, so the `LIMIT` never changes the result — it's \
                  dead clutter that reads as if it mattered."
                .into(),
            remedies: vec![remedy(
                "Drop the LIMIT",
                "EXISTS already short-circuits.",
                "Remove the `LIMIT` from the EXISTS subquery.",
                "Same result, less noise.",
                "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
            )],
        },

        "insert-select-star" => Parts {
            title: "INSERT ... SELECT * maps columns by position".into(),
            what: "An `INSERT INTO t SELECT *` copies rows using `*`.".into(),
            why: "The source columns line up with the target by position, so the statement depends \
                  on both tables' exact column order — a schema change on either side misroutes \
                  values or breaks the insert."
                .into(),
            remedies: vec![remedy(
                "Name the columns on both sides",
                "Make the mapping explicit.",
                "List the target columns and select them by name.",
                "The insert stays correct across schema changes.",
                "INSERT INTO t (a, b) SELECT a, b FROM u",
            )],
        },

        "order-by-nullable-without-nulls" => Parts {
            title: "ORDER BY on a nullable column has portable-NULL issues".into(),
            what: "An `ORDER BY` on a nullable column omits `NULLS FIRST`/`NULLS LAST`.".into(),
            why: "Where NULLs sort is engine-specific — Postgres puts them last on ascending \
                  sorts, MySQL and SQLite put them first — so the same query orders rows \
                  differently across databases."
                .into(),
            remedies: vec![remedy(
                "State the NULL placement",
                "Make NULL ordering explicit and portable.",
                "Add `NULLS FIRST`/`NULLS LAST` (Postgres/SQLite/SQL Server), or on MySQL sort by `col IS NULL` first.",
                "Row order is the same on every engine.",
                "SELECT * FROM t ORDER BY updated_at DESC NULLS LAST",
            )],
        },

        "window-in-where" => Parts {
            title: "A window function can't be used in WHERE/HAVING".into(),
            what: "A window function (`… OVER (…)`) appears in a `WHERE` or `HAVING` clause.".into(),
            why: "Window functions are computed after `WHERE`/`GROUP BY`/`HAVING`, so referencing \
                  one there is a hard error on every engine."
                .into(),
            remedies: vec![remedy(
                "Filter it in an outer query",
                "Compute the window value first, then filter on it.",
                "Move the window expression into a subquery/CTE and filter its output column outside.",
                "The window runs where it's allowed, and the filter sees its result.",
                "SELECT * FROM (SELECT *, row_number() OVER (…) AS rn FROM t) q WHERE rn = 1",
            )],
        },

        "distinct-in-window" => Parts {
            title: "DISTINCT isn't allowed in a window function".into(),
            what: "A windowed aggregate uses `DISTINCT` (`COUNT(DISTINCT x) OVER (…)`).".into(),
            why: "Postgres and most engines reject `DISTINCT` inside a window aggregate — only a \
                  plain aggregate is allowed over a window."
                .into(),
            remedies: vec![remedy(
                "Pre-aggregate, then window",
                "Compute the distinct part without the window.",
                "Aggregate the distinct value in a subquery, then apply the window over that.",
                "The query runs on engines that reject DISTINCT-in-window.",
                "SELECT b, cnt FROM (SELECT b, count(DISTINCT x) AS cnt FROM t GROUP BY b) q",
            )],
        },

        "window-rank-without-order" => Parts {
            title: "A ranking window function has no ORDER BY".into(),
            what: "`row_number()`/`rank()`/etc. `OVER (…)` has no `ORDER BY` inside.".into(),
            why: "Ranking with no defined order produces arbitrary, run-to-run-unstable numbers — \
                  almost always a missing `ORDER BY`."
                .into(),
            remedies: vec![remedy(
                "Order the window",
                "Rank by a defined key.",
                "Add `ORDER BY <key>` (with a unique tiebreaker) inside the `OVER (…)`.",
                "The ranking is well-defined and reproducible.",
                "row_number() OVER (PARTITION BY dept ORDER BY hired_at, id)",
            )],
        },

        "order-by-aggregate-without-group-by" => Parts {
            title: "An aggregate in ORDER BY needs a GROUP BY".into(),
            what: "An aggregate appears in `ORDER BY` alongside a bare column and no `GROUP BY`.".into(),
            why: "The aggregate makes the query grouped, so a non-aggregated SELECT column no longer \
                  has one value per row — the engine rejects it."
                .into(),
            remedies: vec![remedy(
                "Add the GROUP BY, or rethink the sort",
                "Group the selected columns, or don't aggregate in ORDER BY.",
                "Add `GROUP BY <selected columns>`, or use a per-row expression to sort by.",
                "The query type-checks and runs.",
                "SELECT dept, count(*) FROM t GROUP BY dept ORDER BY count(*) DESC",
            )],
        },

        "insert-values-count-mismatch" => Parts {
            title: "INSERT value count doesn't match the columns".into(),
            what: "A `VALUES` row has a different number of values than the named column list.".into(),
            why: "The column list and each row must line up one-to-one, or the insert fails.".into(),
            remedies: vec![remedy(
                "Line up values with columns",
                "One value per named column, per row.",
                "Add the missing values (or remove the extras) so each row matches the column count.",
                "The insert runs.",
                "INSERT INTO t (a, b) VALUES (1, 2)",
            )],
        },

        "update-column-to-self" => Parts {
            title: "Assigning a column to itself does nothing".into(),
            what: "An `UPDATE` has `SET x = x`.".into(),
            why: "Writing a column's own value back is a no-op — usually a typo for a different \
                  right-hand side, or leftover from editing the SET list."
                .into(),
            remedies: vec![remedy(
                "Set the intended value",
                "Point the assignment at what you meant.",
                "Fix the right-hand side, or drop the assignment if it isn't needed.",
                "The UPDATE actually changes something.",
                "UPDATE t SET updated_at = now() WHERE id = 1",
            )],
        },

        "ilike-portability" => Parts {
            title: "ILIKE is Postgres-only".into(),
            what: "The query uses `ILIKE` for case-insensitive matching.".into(),
            why: "`ILIKE` is a Postgres extension; other engines reject it or handle \
                  case-insensitivity differently (MySQL's `LIKE` is already case-insensitive, SQL \
                  Server depends on collation)."
                .into(),
            remedies: vec![remedy(
                "Lower-case both sides",
                "Portable case-insensitive matching.",
                "Replace `col ILIKE pattern` with `LOWER(col) LIKE LOWER(pattern)`.",
                "Matches case-insensitively on every engine.",
                "WHERE LOWER(email) LIKE LOWER('%@Example.com')",
            )],
        },

        "ifnull-portability" => Parts {
            title: "IFNULL isn't portable — use COALESCE".into(),
            what: "The query uses `IFNULL(a, b)`.".into(),
            why: "`IFNULL` exists in MySQL and SQLite but not Postgres or SQL Server. `COALESCE` is \
                  the standard equivalent and works everywhere."
                .into(),
            remedies: vec![remedy(
                "Use COALESCE",
                "The standard, portable spelling.",
                "Replace `IFNULL(a, b)` with `COALESCE(a, b)`.",
                "Same result on every engine, and it takes more arguments if needed.",
                "SELECT COALESCE(nickname, name) FROM users",
            )],
        },

        _ => return None,
    })
}

fn resolve_title(rule: &str) -> &'static str {
    match rule {
        "unknown-table" => "Unknown table",
        "ambiguous-column" => "Ambiguous column",
        "ambiguous-table" => "Ambiguous table",
        "unknown-table-alias" => "Unknown table alias",
        _ => "Unknown column",
    }
}

// --- skeletons for the dialect-divergent rules: shared prose, with the engine-specific remedy
//     supplied by the dialect module so the difference lives in one place per dialect. ---

/// `leading-wildcard-like`: a shared anchor remedy plus the engine's substring-search remedy.
pub(super) fn leading_wildcard(substring: Remedy) -> Parts {
    Parts {
        title: "LIKE '%...' cannot use an index".into(),
        what: "The `LIKE` pattern starts with a wildcard, such as `'%term'`.".into(),
        why: "A B-tree index is ordered by prefix, so a leading wildcard gives it nothing to seek \
              on and the engine scans every row."
            .into(),
        remedies: vec![
            remedy(
                "Anchor the pattern if you can",
                "A prefix-only pattern such as `'term%'` can use a normal index.",
                "Rewrite `'%term'` as `'term%'` when matching the start of the value is enough.",
                "A prefix match seeks into the B-tree instead of scanning every row.",
                "WHERE name LIKE 'term%'",
            ),
            substring,
        ],
    }
}

/// `large-in-list`: shared prose plus the engine's preferred way to pass many values.
pub(super) fn large_in_list(parameterize: Remedy) -> Parts {
    Parts {
        title: "Very long inline IN list".into(),
        what: "An `IN (...)` list holds many inline values, usually generated by the application."
            .into(),
        why: "It bloats the statement, costs parse and plan time that grows with the list, and is \
              not portable because several engines cap the list length."
            .into(),
        remedies: vec![parameterize],
    }
}

/// `order-by-random`: shared prose (with the engine's random function named) plus its sampling
/// remedy.
pub(super) fn order_by_random(func: &str, sample: Remedy) -> Parts {
    Parts {
        title: "ORDER BY random sorts the whole table".into(),
        what: format!("The query sorts by `{func}` to shuffle rows, usually to pick a random few."),
        why: "It computes a random value for every row and sorts the entire result before \
              returning anything, which is expensive on a large table."
            .into(),
        remedies: vec![sample],
    }
}

/// `string-numeric-compare`: shared title/what; the engine's consequence (`why`) and fix differ.
pub(super) fn string_numeric_compare(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "Comparing a text column to a number".into(),
        what: "A text or varchar column is compared directly to a numeric literal.".into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

/// Skeleton for `like-on-numeric-column`: the `why` differs by dialect (Postgres errors, MySQL
/// casts and scans).
pub(super) fn like_on_numeric(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "LIKE against a numeric column".into(),
        what: "A `LIKE` pattern is matched against a numeric column.".into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

/// Skeleton for `select-non-grouped-column`: the `why` differs by dialect (Postgres/MySQL/SQL
/// Server reject it, SQLite returns an arbitrary row's value).
/// Skeleton for `order-by-not-in-distinct-select`: Postgres/SQL Server reject it, MySQL/SQLite run
/// it with an arbitrary ordering.
pub(super) fn order_by_not_in_distinct(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "ORDER BY a column not in the SELECT DISTINCT list".into(),
        what: "A `SELECT DISTINCT` orders by a column that isn't in the select list.".into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

pub(super) fn select_non_grouped(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "A SELECT column is neither grouped nor aggregated".into(),
        what: "In a grouped query, a SELECT-list column is not in `GROUP BY` and not inside an \
               aggregate."
            .into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

/// `join-type-mismatch`: shared title/what; the engine's consequence and fix differ.
pub(super) fn join_type_mismatch(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "Join compares columns of different types".into(),
        what: "A join condition equates columns whose types differ, such as an integer key to a \
               text key."
            .into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

/// `integer-division`: shared title/what; the engine's result (truncate vs decimal) and fix differ.
pub(super) fn integer_division(why: &str, fix: Remedy) -> Parts {
    Parts {
        title: "Dividing two integers".into(),
        what: "Two integer operands are divided with `/`.".into(),
        why: why.into(),
        remedies: vec![fix],
    }
}

/// `not-equals-excludes-null`: shared prose; only the null-safe inequality differs per engine.
pub(super) fn not_equals_excludes_null(fix: Remedy) -> Parts {
    Parts {
        title: "!= silently excludes NULL rows".into(),
        what: "A `<>` (or `!=`) predicate is used on a column that can be NULL.".into(),
        why: "`col <> v` is unknown when `col` is NULL, so rows where the column is NULL are \
              dropped from the result. Often those rows should be included."
            .into(),
        remedies: vec![fix],
    }
}

/// `non-sargable-predicate`: a shared bare-column remedy plus the engine's expression-index remedy.
pub(super) fn non_sargable(expr_index: Remedy) -> Parts {
    Parts {
        title: "Function on the column defeats the index".into(),
        what: "A function or arithmetic wraps a column in `WHERE`/`JOIN`, so an index on that \
               column cannot be used."
            .into(),
        why: "The index stores the raw column value. Once it is wrapped in an expression the engine \
              must compute the expression for every row and scan."
            .into(),
        remedies: vec![
            remedy(
                "Compare the bare column",
                "Move the function off the column.",
                "Rewrite `WHERE lower(email) = 'x'` as `WHERE email = 'x'` when the data is already \
                 normalized, or store a normalized column.",
                "A bare column matches the index directly.",
                "WHERE email = 'x'",
            ),
            expr_index,
        ],
    }
}

/// `function-on-indexed-column`: a shared bare-column remedy plus the engine's expression-index
/// remedy (the same expression-index syntax as `non_sargable`).
pub(super) fn function_on_indexed(expr_index: Remedy) -> Parts {
    Parts {
        title: "Function on an indexed column".into(),
        what: "A function is applied to a column that has an index, so the index cannot be used."
            .into(),
        why: "The index is on the raw column. Wrapping it in a function forces the engine to \
              compute the function per row and scan instead of seeking."
            .into(),
        remedies: vec![
            remedy(
                "Apply the function to the literal, not the column",
                "Turn a function-on-column into a range on the bare column.",
                "Rewrite `WHERE date(created) = '2026-01-01'` as a range: \
                 `created >= '2026-01-01' AND created < '2026-01-02'`.",
                "The bare column seeks the index and the range avoids the per-row function.",
                "WHERE created >= '2026-01-01' AND created < '2026-01-02'",
            ),
            expr_index,
        ],
    }
}
