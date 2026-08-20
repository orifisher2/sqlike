//! DuckDB-specific copy. Everything not here falls through to [`super::common`].
//!
//! DD1 shipped this file empty on purpose: seeding it from `postgres.rs` would have copied index
//! remedies for a structure DuckDB barely uses. DD3a fills in the sargability family from real
//! measurements (DuckDB v1.5.5, 491 520 rows spanning four row groups, counting
//! `cumulative_rows_scanned`), and with it the vocabulary the rest of the DuckDB copy inherits:
//!
//! - **zone map** — the min/max kept per column per row group. It is what prunes.
//! - **row group** — 122 880 rows, the unit a zone map excludes.
//! - **clustering** / *physically ordered by* — the property that makes pruning possible. Never
//!   "indexed": DuckDB's only index is the ART, and it is a point-lookup structure.
//! - **prune** vs **scan** — pruning is what the zone map does to a row group; scanning is what
//!   is left. The measurable is rows scanned, so the copy names it.
//!
//! "Index" survives in exactly one place, and only because it was measured: an equality on an
//! ART-indexed column read **1 row out of 491 520**, while the same index left a range scan
//! unchanged at 96 256. Everything else lost the word rather than softening it.

use super::{common, remedy, Finding, Parts, Remedy};

pub(super) fn rich(f: &Finding) -> Option<Parts> {
    Some(match f.rule.as_str() {
        "non-sargable-predicate" => Parts {
            title: "Wrapping the column blocks row-group pruning".into(),
            what: "A function or cast wraps a column in `WHERE`/`JOIN`, so the comparison is \
                   against a computed value rather than the stored one."
                .into(),
            why:
                "DuckDB keeps a min and a max per column per row group and skips whole row \
                  groups whose range cannot match. That test needs the stored value, so wrapping \
                  the column in a function forces every row group to be read, where the bare \
                  comparison reads only the groups whose range covers the value. Simple arithmetic \
                  is not affected, because DuckDB rewrites `col + 1 = 5001` back into `col = 5000`."
                    .into(),
            remedies: vec![rewrite_to_bare_column()],
        },

        "function-on-indexed-column" => Parts {
            title: "The function hides the column from pruning".into(),
            what: "A function is applied to a column the table is ordered by.".into(),
            why: "Physical order is what lets DuckDB skip row groups, and it can only compare the \
                  stored values. Once the column is wrapped, the ordering stops being usable and \
                  the scan reads everything."
                .into(),
            remedies: vec![rewrite_to_bare_column()],
        },

        "unindexed-filter" => Parts {
            title: "The filtered column is not the clustering key".into(),
            what: "The query filters on a column the table is not physically ordered by.".into(),
            why: "Row groups are skipped by comparing the filter against each group's stored min \
                  and max, which only narrows anything when the values are clustered together. \
                  On an unordered column the matching rows are spread across every row group, so \
                  none can be skipped and the whole table is read."
                .into(),
            remedies: vec![
                remedy(
                    "Store the table in the order you filter it",
                    "Physical order is what makes a filter cheap here.",
                    "Rebuild the table with `CREATE TABLE … AS SELECT … ORDER BY <column>`, or \
                     insert in that order.",
                    "Matching rows end up in a few row groups instead of all of them, and the \
                     rest are skipped without being read.",
                    "CREATE TABLE t AS SELECT * FROM t_old ORDER BY created_at",
                ),
                remedy(
                    "Or add an index if the filter is a point lookup",
                    "An index is worth it for equality on one value, not for ranges.",
                    "`CREATE INDEX … ON t (col)` builds an ART index, which serves `col = ?`.",
                    "An indexed equality reads the matching row instead of scanning, but the same \
                     index leaves a range scan unchanged, so this helps point lookups only.",
                    "CREATE INDEX idx_t_col ON t (col)",
                ),
            ],
        },

        "or-defeats-index" => Parts {
            title: "The OR spans a column the table is not ordered by".into(),
            what: "An `OR` combines a predicate on the clustering key with one on another column."
                .into(),
            why: "A row group can only be skipped when *every* branch of the `OR` rules it out. \
                  One branch on an unordered column is enough to make that impossible, so the \
                  whole table is read — while an `OR` whose branches both sit on the clustering \
                  key still prunes."
                .into(),
            remedies: vec![remedy(
                "Split the branches, or keep them on the clustering key",
                "Each branch can prune on its own; together they cannot.",
                "Rewrite as a `UNION ALL` of one query per branch, or order the table so both \
                 branches sit on the clustering key.",
                "Each branch is then evaluated against the zone map separately, so each can skip \
                 the row groups it cannot match.",
                "SELECT … WHERE k < 100 UNION ALL SELECT … WHERE other = 3",
            )],
        },

        "index-prefix-mismatch" => Parts {
            title: "The filter skips the leading clustering column".into(),
            what: "The table is ordered by several columns and the query filters on a later one \
                   without constraining the first."
                .into(),
            why: "Ordering by `(a, b)` sorts by `b` only inside a run of equal `a`, so a filter \
                  on `b` alone matches rows in nearly every row group and prunes far less than the \
                  same filter on the leading column."
                .into(),
            remedies: vec![remedy(
                "Constrain the leading column too, or reorder the table",
                "Ordering has a prefix, the same way a composite index does.",
                "Add a predicate on the leading column, or rebuild the table ordered by the \
                 column you actually filter on first.",
                "The scan can then skip the row groups whose leading-column range cannot match.",
                "CREATE TABLE t AS SELECT * FROM t_old ORDER BY b, a",
            )],
        },

        "inequality-defeats-index" => Parts {
            title: "`<>` cannot skip any row group".into(),
            what: "The predicate excludes a value with `<>` or `NOT IN` rather than selecting one."
                .into(),
            why: "A row group's min and max can prove it holds no matching value, but never that \
                  it holds no *differing* one — any group with more than one distinct value has \
                  some. So an inequality reads every row group, where the matching equality reads \
                  only the ones whose range covers the value."
                .into(),
            remedies: vec![remedy(
                "Say which values you want",
                "A positive predicate has a range to compare against.",
                "Replace `col <> x` with the range or the list of values you actually want.",
                "The zone map can then rule out the row groups outside that range.",
                "WHERE status IN ('open', 'pending')",
            )],
        },

        // Not `common::leading_wildcard`: its own text explains the B-tree it defeats, which is
        // the right story on the other five engines and the wrong one here.
        "leading-wildcard-like" => Parts {
            title: "A leading wildcard reads every row group".into(),
            what: "The `LIKE` pattern starts with a wildcard, such as `'%term'`.".into(),
            why: "DuckDB keeps the smallest and largest value of each column per row group, which \
                  is enough to rule a group out for a pattern anchored at the start but says \
                  nothing about what a value ends with. An anchored `LIKE 'term%'` prunes to the \
                  groups whose range covers the prefix, a leading-wildcard `LIKE '%term'` reads \
                  every one."
                .into(),
            remedies: vec![remedy(
                "Anchor the pattern, or store what you search on",
                "An anchored pattern is comparable against each row group's range.",
                "Use `LIKE 'term%'` where matching the start is enough. For true substring \
                 search, store the normalized or reversed form as its own column and order the \
                 table by it.",
                "The scan can then skip the row groups whose value range cannot contain a match.",
                "WHERE name LIKE 'smith%'",
            )],
        },

        "like-on-numeric-column" => common::like_on_numeric(
            "DuckDB has no implicit number-to-text conversion for `LIKE`, so this raises a binder \
             error and the query will not run.",
            remedy(
                "Compare like types",
                "Use a numeric comparison, or store the value as text.",
                "For a numeric match use `acct = 4001`; cast explicitly with `CAST(acct AS \
                 VARCHAR) LIKE '4%'` only if you really want a textual match.",
                "Matching types let the query run. Note that the explicit cast still reads every \
                 row group, because the comparison is no longer against the stored value.",
                "WHERE acct = 4001",
            ),
        ),

        "string-numeric-compare" => common::string_numeric_compare(
            "DuckDB coerces the two sides and runs the query, so it does not fail — but the \
             comparison is against a converted value, which reads every row group, and the \
             conversion can match rows you did not mean (`'01'` equals `1`).",
            remedy(
                "Compare text to text",
                "Quote the value, or store the column as a number.",
                "Write `code = '123'` to compare as text, or change the column to a numeric type \
                 if that is what it holds.",
                "Matching types compare the stored values, so row groups can be skipped and no \
                 surprising conversion happens.",
                "WHERE code = '123'",
            ),
        ),

        // DD3e. `common`'s version says the index cannot be used; here the casualty is row-group
        // pruning, and it is the *unknown* pattern that makes it unpredictable.
        "parameterized-like-pattern" => Parts {
            title: "A parameterized LIKE may or may not be able to skip anything".into(),
            what: "A `LIKE` compares against a parameter, so the pattern is unknown and may start \
                   with `%`."
                .into(),
            why: "An anchored pattern lets DuckDB compare against each row group's stored range \
                  and skip the ones that cannot match. A leading wildcard leaves nothing to \
                  compare, so the whole table is read. Because the pattern arrives at runtime, the \
                  same query can do either, and nothing in the SQL says which."
                .into(),
            remedies: vec![remedy(
                "Anchor the pattern, or validate the input",
                "Make the shape of the pattern a property of the query, not of the input.",
                "Bind a prefix pattern (`:prefix || '%'`) when prefix matching is enough, or store \
                 a normalized column for true substring search and order the table by it.",
                "An anchored pattern can always be compared against a row group's range, so the \
                 cost stops depending on what the caller passed.",
                "WHERE name LIKE :prefix || '%'",
            )],
        },

        // DD3d. The one rule whose severity DuckDB *raises*, so the copy has to explain why the
        // same query is a bigger deal here than on a row store.
        "select-star" => Parts {
            title: "Every column you select is a column that gets read".into(),
            what: "The query selects all columns with `*` rather than naming the ones it uses."
                .into(),
            why: "DuckDB stores each column separately, so a query reads exactly the columns it \
                  projects and pays nothing for the rest. `SELECT *` gives that up and reads every \
                  column, which on a wide table costs several times what naming one column does — \
                  while adding a single narrow column to the list is close to free. A row store \
                  fetches the whole row off the page whatever you project, so this matters more \
                  here than it does there."
                .into(),
            remedies: vec![remedy(
                "Name the columns you use",
                "The projection list is the read list on a columnar engine.",
                "Replace `*` with the columns the query actually needs, especially when the table \
                 is wide or holds large text or blob columns.",
                "Unnamed columns are never read from storage at all, so the scan shrinks in \
                 proportion to what you left out.",
                "SELECT id, created_at, status FROM events",
            )],
        },

        // DD3d. `common`'s version is written for engines where deep pagination falls off an
        // index-ordered fast path. DuckDB has no such path, so it degrades least of the six —
        // the finding is real but it is a ~5x improvement, not an escape from a cliff.
        "offset-pagination" => Parts {
            title: "A deep OFFSET still walks everything it skips".into(),
            what: "The query pages with `OFFSET`, so reaching a late page means producing and \
                   discarding every row before it."
                .into(),
            why: "DuckDB has to order and count past the skipped rows before it can return the \
                  page, so a late page costs more than an early one. The gap is smaller here than \
                  on an engine that serves the first page through an index on the ordering column: \
                  those start out near-instant and fall a long way at depth, while DuckDB scans \
                  for the first page too and degrades more gently from there."
                .into(),
            remedies: vec![remedy(
                "Page by the last key you saw",
                "Ask for rows after a value instead of skipping a count.",
                "Keep the last row's ordering key and query `WHERE key > :last ORDER BY key LIMIT \
                 n`, rather than increasing `OFFSET`.",
                "The skipped rows are never produced, so a late page costs what an early one does \
                 — and unlike OFFSET it does not get slower the further the user pages.",
                "SELECT * FROM events WHERE id > 150000 ORDER BY id LIMIT 20",
            )],
        },

        // DD3b. `common`'s version explains a lost index lookup and calls the lost hash join
        // "secondary" — precisely backwards here, where the hash join is the only mechanism.
        "or-in-join-on" => Parts {
            title: "An OR in the join condition rules out a hash join".into(),
            what:
                "A join `ON` condition contains `OR`, so the match is no longer a plain equality."
                    .into(),
            why: "DuckDB joins by building a hash table on one side and probing it with the \
                  other, which needs an equality to hash. An `OR` leaves it comparing every pair \
                  of rows instead, so the cost grows with the product of the two tables rather \
                  than their sum. Splitting the join into one branch per condition returns the \
                  same rows and, on a large join, runs in a fraction of the time."
                .into(),
            remedies: vec![remedy(
                "Split into one join per branch",
                "Each branch is an equality, so each can be hashed.",
                "Rewrite as a `UNION ALL` of one join per `OR` branch, guarding the later \
                 branches so a row matching two of them is not counted twice.",
                "Every branch becomes a hash join instead of a full pairwise comparison, which is \
                 the largest improvement any rewrite in this catalog has been measured to give.",
                "SELECT … FROM a JOIN b ON b.x = a.x UNION ALL SELECT … FROM a JOIN b ON b.y = a.y \
                 AND NOT (b.x = a.x)",
            )],
        },

        // Same reason as `leading-wildcard-like`: `common`'s version explains a B-tree.
        "implicit-cast-in-filter" => Parts {
            title: "The literal's type forces a per-row conversion".into(),
            what: "A column is compared to a value of a different type, so the engine converts \
                   the column rather than the value."
                .into(),
            why: "Converting per row means the comparison is no longer against the stored values, \
                  and those are what DuckDB's per-row-group min and max describe — so no row \
                  group can be skipped, where the same comparison against a matching-type literal \
                  prunes. An integer column compared to a fractional literal is usually a mistake \
                  in its own right, since it can never match."
                .into(),
            remedies: vec![remedy(
                "Write the literal in the column's type",
                "Convert the value, never the column.",
                "Write `id = 5` rather than `id = 5.0` or `id = '5'`.",
                "The comparison is then against the stored values, so row groups that cannot \
                 match are skipped instead of read.",
                "WHERE id = 5",
            )],
        },

        "join-type-mismatch" => common::join_type_mismatch(
            "DuckDB coerces the two sides rather than raising an error, so the join runs — but \
             the conversion happens per row and can equate values you did not mean to match.",
            remedy(
                "Make the join columns the same type",
                "Fix the schema, or cast one side explicitly.",
                "Align the column types (preferred), or cast in the join so the conversion is at \
                 least visible: `ON a.id = CAST(b.id AS BIGINT)`.",
                "Matching types remove the per-row conversion and the silent-match risk.",
                "ON a.id = CAST(b.id AS BIGINT)",
            ),
        ),

        // The deferral is an index-seek story: select the page from a narrow index, then look up
        // only those rows. DuckDB has no such seek to defer to, and nothing is measured here yet
        // (DD5), so `defer_pays_off` withholds the fix and this copy claims no speed-up.
        "deferred-join-pagination" => Parts {
            title: "Paginated query joins before it limits".into(),
            what: "A paginated query joins extra tables before applying `LIMIT`, so the join runs \
                   for rows that are then discarded."
                .into(),
            why: "The usual fix for this shape is to pick the page's keys from an index first and \
                  join only to those. That reasoning assumes an index seek, which is not how \
                  DuckDB reads a table: it scans column segments and prunes them by zone map. The \
                  rewrite has not been measured here, so it is reported but not offered."
                .into(),
            remedies: vec![remedy(
                "Narrow the candidate set before paginating",
                "Give the scan something to prune on instead of reordering the joins.",
                "Filter on a column whose zone maps can skip row groups, so the page is chosen \
                 from a fraction of the table, and prefer keyset pagination over a deep `OFFSET`.",
                "Pruning removes row groups from the scan entirely, which is the lever this \
                 storage model actually offers.",
                "WHERE created >= :since ORDER BY created DESC LIMIT 20",
            )],
        },

        _ => return None,
    })
}

/// Every wrapper finding has the same fix here: compare the stored column.
fn rewrite_to_bare_column() -> Remedy {
    remedy(
        "Compare the column itself",
        "Move the work to the other side of the comparison.",
        "Rewrite so the bare column faces the constant — `created_at >= DATE '2024-01-01'` rather \
         than `year(created_at) = 2024`. If the computed form is what you query by, store it as a \
         column and order the table by it.",
        "The comparison is then against the stored values, which is what a row group's min and \
         max describe, so groups that cannot match are skipped instead of read.",
        "WHERE created_at >= DATE '2024-01-01' AND created_at < DATE '2025-01-01'",
    )
}
