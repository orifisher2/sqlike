//! Base-table extraction for the web stats form — the tables a caller could supply a
//! row count for. Excludes CTE names (a `FROM cte` parses as a base table but isn't one)
//! and derived/table-function sources; recurses into subqueries.

use std::collections::HashSet;

use super::query::Query;
use super::stage::{From, Relation, RelationRef};

/// Base-table names referenced in FROM/JOIN across the query, first-seen order, deduped.
pub fn base_table_names(query: &Query) -> Vec<String> {
    let mut ctes: HashSet<String> = query
        .ctes
        .iter()
        .map(|c| c.name.text.to_ascii_lowercase())
        .collect();
    let (mut out, mut seen) = (Vec::new(), HashSet::new());
    for c in &query.ctes {
        walk(&c.query, &mut ctes, &mut out, &mut seen);
    }
    walk(&query.body, &mut ctes, &mut out, &mut seen);
    out
}

fn walk(
    rel: &Relation,
    ctes: &mut HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match rel {
        Relation::Stage(s) => {
            ctes.extend(s.ctes.iter().map(|c| c.name.text.to_ascii_lowercase()));
            for c in &s.ctes {
                walk(&c.query, ctes, out, seen);
            }
            if let Some(from) = &s.from {
                walk_from(from, ctes, out, seen);
            }
        }
        Relation::SetOp(so) => {
            walk(&so.left, ctes, out, seen);
            walk(&so.right, ctes, out, seen);
        }
    }
}

fn walk_from(
    from: &From,
    ctes: &mut HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match from {
        From::Relation(RelationRef::BaseTable { name, .. }) => {
            let key = name.name.text.to_ascii_lowercase();
            if !ctes.contains(&key) && seen.insert(key) {
                out.push(name.name.text.clone());
            }
        }
        From::Relation(RelationRef::Derived { subquery, .. }) => walk(subquery, ctes, out, seen),
        From::Relation(RelationRef::TableFunction { .. }) => {}
        From::Join(j) => {
            walk_from(&j.left, ctes, out, seen);
            walk_from(&j.right, ctes, out, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::translate;
    use crate::model::Analyzed;
    use crate::parser::parse;
    use crate::Dialect;

    fn tables(sql: &str) -> Vec<String> {
        let stmts = parse(sql, Dialect::Postgres).unwrap();
        match translate(&stmts[0]) {
            Analyzed::Query(q) => super::base_table_names(&q),
            _ => vec![],
        }
    }

    #[test]
    fn collects_from_and_joins_dedup_first_seen() {
        assert_eq!(
            tables("SELECT * FROM orders o JOIN users u ON u.id = o.user_id JOIN orders o2 ON o2.id = o.id"),
            ["orders", "users"]
        );
    }

    #[test]
    fn excludes_cte_names_but_keeps_their_base_tables() {
        assert_eq!(
            tables("WITH recent AS (SELECT * FROM orders) SELECT * FROM recent"),
            ["orders"]
        );
    }

    #[test]
    fn recurses_into_subqueries_skips_table_functions() {
        assert_eq!(
            tables("SELECT * FROM (SELECT * FROM events) e, generate_series(1, 10) g"),
            ["events"]
        );
    }

    #[test]
    fn no_from_is_empty() {
        assert!(tables("SELECT 1").is_empty());
    }
}
