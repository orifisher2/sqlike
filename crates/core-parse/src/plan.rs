//! Query-plan model + a Postgres `EXPLAIN (FORMAT JSON)` parser.
//!
//! Pure — JSON text in, a normalized [`Plan`] out, no DB or I/O — so it sits beside `schema`
//! and `tokenize`, and the CLI and the engine consume the same type. The shape is deliberately
//! lean: the only consumers (the `unindexed-*` rules) need, per scan node, the relation, how it
//! was read, and which columns the index *served* vs. merely *filtered*.
//!
//! The Postgres parsing mirrors the proven verify-framework parser (`crates/verify/src/plan.rs`),
//! extended with the column lists (from `Index Cond` / `Filter`) and row counts verification
//! didn't need.

use std::fmt;

use serde_json::Value;

use crate::dialect::Dialect;
use crate::model::expr::Expr;
use crate::model::name::Name;
use crate::model::{translate, Analyzed, Relation};
use crate::parser::parse;

/// A parsed query plan. Serializable so the client can tokenize its identifiers and ship the
/// structured plan to the server (v0.3.8.x) — the raw `EXPLAIN` JSON never travels.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    pub root: PlanNode,
    /// `EXPLAIN ANALYZE` — actual row counts are present, so findings may say "in this run".
    pub analyzed: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanNode {
    pub access: Access,
    pub relation: Option<Name>,
    pub alias: Option<Name>,
    /// Columns the index served (from `Index Cond`) — index-supported.
    pub index_keys: Vec<Name>,
    /// Columns filtered after the scan (from `Filter`) — not index-supported.
    pub filtered: Vec<Name>,
    pub est_rows: Option<u64>,
    pub actual_rows: Option<u64>,
    pub children: Vec<PlanNode>,
}

/// How a node reads its relation. Non-scan nodes (joins, sorts, …) are [`Access::Other`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Access {
    SeqScan,
    /// A plain, index-only, or bitmap index scan — all "an index served a column".
    IndexScan {
        index: Option<Name>,
    },
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NotJson(String),
    NotXml(String),
    NoPlan,
    UnsupportedDialect(Dialect),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::NotJson(e) => write!(f, "EXPLAIN is not valid JSON: {e}"),
            PlanError::NotXml(e) => write!(f, "SHOWPLAN_XML is not valid XML: {e}"),
            PlanError::NoPlan => write!(
                f,
                "EXPLAIN JSON has no top-level `Plan` (expected `EXPLAIN (FORMAT JSON)`)"
            ),
            PlanError::UnsupportedDialect(d) => {
                write!(f, "EXPLAIN input isn't supported for {d:?} yet")
            }
        }
    }
}

impl std::error::Error for PlanError {}

impl Plan {
    /// Parse an `EXPLAIN` document for `dialect` into the shared model. The `text` is JSON for
    /// Postgres/MySQL/SQLite and XML (`SHOWPLAN_XML`) for SQL Server — the one non-JSON dialect.
    pub fn from_explain(text: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        match dialect {
            Dialect::Postgres => Self::from_pg_explain_json(text, dialect),
            Dialect::Mysql => Self::from_mysql_explain_json(text, dialect),
            Dialect::Sqlite => Self::from_sqlite_query_plan(text, dialect),
            Dialect::Mssql => Self::from_mssql_showplan_xml(text, dialect),
        }
    }

    /// Parse a Postgres `EXPLAIN (FORMAT JSON)` document — the `[{ "Plan": … }]` array.
    pub fn from_pg_explain_json(json: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let v: Value = serde_json::from_str(json).map_err(|e| PlanError::NotJson(e.to_string()))?;
        let plan = v
            .get(0)
            .and_then(|e| e.get("Plan"))
            .ok_or(PlanError::NoPlan)?;
        let mut analyzed = false;
        let root = parse_node(plan, dialect, &mut analyzed);
        Ok(Plan { root, analyzed })
    }

    /// Parse a MySQL `EXPLAIN FORMAT=JSON` document (a `query_block`). Each table access becomes a
    /// leaf under a synthetic `Query` root — the verdict only walks scan nodes. `EXPLAIN ANALYZE`
    /// isn't JSON on MySQL, so there are no actual rows.
    pub fn from_mysql_explain_json(json: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let v: Value = serde_json::from_str(json).map_err(|e| PlanError::NotJson(e.to_string()))?;
        v.get("query_block").ok_or(PlanError::NoPlan)?;
        let mut children = Vec::new();
        collect_mysql_tables(&v, dialect, &mut children);
        Ok(Plan {
            root: query_root(children),
            analyzed: false,
        })
    }

    /// Parse a SQL Server `SET SHOWPLAN_XML ON` document. Each table-access `<RelOp>` becomes a
    /// leaf under a synthetic `Query` root: a *Scan* op (`Table Scan`, `Clustered Index Scan`, full
    /// `Index Scan`) is a full scan → Seq Scan; a *Seek*/`Key Lookup`/`RID Lookup` reads through an
    /// index → Index Scan, its served columns the seek keys. Estimated plan → no actual rows.
    pub fn from_mssql_showplan_xml(xml: &str, _dialect: Dialect) -> Result<Plan, PlanError> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| PlanError::NotXml(e.to_string()))?;
        let children: Vec<PlanNode> = doc
            .descendants()
            .filter(|n| n.has_tag_name("RelOp"))
            .filter_map(mssql_reloop_node)
            .collect();
        Ok(Plan {
            root: query_root(children),
            analyzed: false,
        })
    }

    /// Parse a SQLite `EXPLAIN QUERY PLAN` document as its `.mode json` rows —
    /// `[{"detail":"SCAN t"}, …]` (`id`/`parent`/`notused` ignored). The thinnest dialect: shape
    /// only, no costs. `SCAN` → full scan, `SEARCH … (col=?)` → an index seek serving `col`.
    pub fn from_sqlite_query_plan(json: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let rows: Vec<Value> =
            serde_json::from_str(json).map_err(|e| PlanError::NotJson(e.to_string()))?;
        let children: Vec<PlanNode> = rows
            .iter()
            .filter_map(|r| r.get("detail").and_then(Value::as_str))
            .filter_map(|d| sqlite_node(d, dialect))
            .collect();
        Ok(Plan {
            root: query_root(children),
            analyzed: false,
        })
    }

    /// Every node, root first (pre-order) — what the verdict logic walks.
    pub fn nodes(&self) -> Vec<&PlanNode> {
        let mut out = Vec::new();
        collect(&self.root, &mut out);
        out
    }

    /// What this plan says about a missing-index finding on a column (all names normalized). A node
    /// matches the occurrence when either of its names (`relation`/`alias`) equals either the base
    /// `table` or the query `alias` — engines disagree on which they report (Postgres both, MySQL
    /// the alias as relation, SQL Server the base table in `<Object>`), so the intersection covers
    /// all of them. `Suppress` when an index served the column; `Confirm` when it was scanned
    /// without one; `NoSignal` otherwise — including two matching nodes that disagree (a bare-table
    /// self-join we can't disambiguate).
    pub fn verdict(&self, table: &str, alias: Option<&str>, column: &str) -> Verdict {
        let is_target =
            |nm: &Name| nm.normalized() == table || alias.is_some_and(|a| nm.normalized() == a);
        let on_target = |n: &PlanNode| {
            n.relation.as_ref().is_some_and(&is_target) || n.alias.as_ref().is_some_and(&is_target)
        };
        let on_table: Vec<&PlanNode> = self.nodes().into_iter().filter(|n| on_target(n)).collect();
        if on_table.is_empty() {
            return Verdict::NoSignal;
        }
        let has = |cols: &[Name]| cols.iter().any(|c| c.normalized() == column);
        let served = on_table
            .iter()
            .any(|n| matches!(n.access, Access::IndexScan { .. }) && has(&n.index_keys));
        let scanned = on_table
            .iter()
            .find(|n| matches!(n.access, Access::SeqScan) || has(&n.filtered));
        match (served, scanned) {
            (true, None) => Verdict::Suppress,
            (false, Some(n)) => Verdict::Confirm {
                actual_rows: n.actual_rows,
            },
            _ => Verdict::NoSignal,
        }
    }
}

/// What a [`Plan`] says about a structural missing-index finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// An index served the column — drop the finding.
    Suppress,
    /// The column was scanned without an index — confirm, and sharpen with the run's rows.
    Confirm { actual_rows: Option<u64> },
    /// No clear signal — leave the structural finding unchanged.
    NoSignal,
}

fn collect<'a>(node: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
    out.push(node);
    for c in &node.children {
        collect(c, out);
    }
}

fn parse_node(v: &Value, dialect: Dialect, analyzed: &mut bool) -> PlanNode {
    let children: Vec<PlanNode> = v
        .get("Plans")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|c| parse_node(c, dialect, analyzed)).collect())
        .unwrap_or_default();

    let node_type = str_field(v, "Node Type").unwrap_or_default();
    let actual_rows = v.get("Actual Rows").and_then(Value::as_u64);
    if actual_rows.is_some() {
        *analyzed = true;
    }

    let mut node = PlanNode {
        access: classify(&node_type, v),
        relation: str_field(v, "Relation Name").map(name),
        alias: str_field(v, "Alias").map(name),
        index_keys: cond_columns(v, "Index Cond", dialect),
        filtered: cond_columns(v, "Filter", dialect),
        est_rows: v.get("Plan Rows").and_then(Value::as_u64),
        actual_rows,
        children,
    };

    // A Bitmap Heap Scan carries the table; its child Bitmap Index Scan carries the index and the
    // condition. Fold the child up so the node reads as one index scan on the relation.
    if node_type == "Bitmap Heap Scan" {
        if let Some(idx) = node
            .children
            .iter()
            .find(|c| matches!(c.access, Access::IndexScan { .. }))
        {
            node.access = idx.access.clone();
            node.index_keys = idx.index_keys.clone();
        }
    }

    node
}

fn classify(node_type: &str, v: &Value) -> Access {
    match node_type {
        "Seq Scan" => Access::SeqScan,
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" => Access::IndexScan {
            index: str_field(v, "Index Name").map(name),
        },
        other => Access::Other(other.to_string()),
    }
}

/// A synthetic non-scan root holding all of a plan's table-access leaves.
fn query_root(children: Vec<PlanNode>) -> PlanNode {
    PlanNode {
        access: Access::Other("Query".to_string()),
        relation: None,
        alias: None,
        index_keys: Vec::new(),
        filtered: Vec::new(),
        est_rows: None,
        actual_rows: None,
        children,
    }
}

/// Walk a MySQL `query_block` tree, emitting one leaf node per table access (including tables
/// reached through nested subqueries). Mirrors the verify-framework walk.
fn collect_mysql_tables(v: &Value, dialect: Dialect, out: &mut Vec<PlanNode>) {
    if let Some(map) = v.as_object() {
        if str_field(v, "table_name").is_some() && str_field(v, "access_type").is_some() {
            out.push(mysql_table_node(v, dialect));
        }
        for child in map.values() {
            collect_mysql_tables(child, dialect, out);
        }
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .for_each(|e| collect_mysql_tables(e, dialect, out));
    }
}

/// One MySQL table access → a scan node. `ALL`/`index` are full scans ("no seek") → `SeqScan` with
/// no served columns; a keyed access (`ref`/`eq_ref`/`range`/`const`) is an index seek whose
/// `used_key_parts` are the served columns. `attached_condition` gives the post-scan filter.
fn mysql_table_node(v: &Value, dialect: Dialect) -> PlanNode {
    let access_type = str_field(v, "access_type").unwrap_or_default();
    let full_scan = matches!(access_type.as_str(), "ALL" | "index");
    let access = if full_scan {
        Access::SeqScan
    } else {
        Access::IndexScan {
            index: str_field(v, "key").map(name),
        }
    };
    // Only an index *seek* serves a column; a full index scan (`access_type: index`) does not, even
    // though MySQL still lists `used_key_parts` — gating here prevents a false suppress.
    let index_keys = if full_scan {
        Vec::new()
    } else {
        v.get("used_key_parts")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str_name).collect())
            .unwrap_or_default()
    };
    let filtered = str_field(v, "attached_condition")
        .map(|c| cond_columns_str(&c, dialect))
        .unwrap_or_default();
    PlanNode {
        access,
        // MySQL reports the alias here when one exists; there's no separate base-table field.
        relation: str_field(v, "table_name").map(name),
        alias: None,
        index_keys,
        filtered,
        est_rows: None,
        actual_rows: None,
        children: Vec::new(),
    }
}

fn str_name(s: &str) -> Name {
    Name::new(s.to_string(), false)
}

/// One SQLite `EXPLAIN QUERY PLAN` detail → a scan node. `SCAN t` is a full scan (a full
/// `USING [COVERING] INDEX` scan is also reported as `SCAN`, so it's correctly a Seq Scan);
/// `SEARCH t USING INDEX ix (col=?)` is an index seek whose served columns are the trailing
/// `(col=?)` clause.
fn sqlite_node(detail: &str, dialect: Dialect) -> Option<PlanNode> {
    if let Some(rest) = detail.strip_prefix("SCAN ") {
        return Some(scan_leaf(
            Access::SeqScan,
            sqlite_first_token(sqlite_strip_table(rest)),
            Vec::new(),
        ));
    }
    if let Some(rest) = detail.strip_prefix("SEARCH ") {
        let rest = sqlite_strip_table(rest);
        let access = Access::IndexScan {
            index: sqlite_index_name(rest),
        };
        return Some(scan_leaf(
            access,
            sqlite_first_token(rest),
            sqlite_seek_columns(rest, dialect),
        ));
    }
    None
}

fn scan_leaf(access: Access, relation: Option<Name>, index_keys: Vec<Name>) -> PlanNode {
    PlanNode {
        access,
        relation,
        alias: None,
        index_keys,
        filtered: Vec::new(),
        est_rows: None,
        actual_rows: None,
        children: Vec::new(),
    }
}

/// Older SQLite prints `SCAN TABLE t`; modern drops `TABLE`. Tolerate both.
fn sqlite_strip_table(s: &str) -> &str {
    s.strip_prefix("TABLE ").unwrap_or(s)
}

/// The table (or its alias) — the first token of a `SCAN`/`SEARCH` detail tail.
fn sqlite_first_token(rest: &str) -> Option<Name> {
    rest.split_whitespace().next().map(str_name)
}

/// The index a `SEARCH … USING …` seeks: a named/covering index, or `INTEGER PRIMARY KEY` for a
/// rowid seek. `None` if the clause names no index.
fn sqlite_index_name(rest: &str) -> Option<Name> {
    let after = &rest[rest.find("USING ")? + "USING ".len()..];
    if let Some(pos) = after.find("INDEX ") {
        after[pos + "INDEX ".len()..]
            .split([' ', '('])
            .find(|s| !s.is_empty())
            .map(str_name)
    } else if after.contains("PRIMARY KEY") {
        Some(str_name("INTEGER PRIMARY KEY"))
    } else {
        None
    }
}

/// The columns a `SEARCH`'s trailing `(col=? …)` clause seeks on — `SEARCH` is always a seek in
/// SQLite, so these are the index-served columns.
fn sqlite_seek_columns(rest: &str, dialect: Dialect) -> Vec<Name> {
    match (rest.rfind('('), rest.rfind(')')) {
        (Some(open), Some(close)) if close > open => cond_columns_str(&rest[open..=close], dialect),
        _ => Vec::new(),
    }
}

/// One SQL Server `<RelOp>` → a scan node, or `None` if it isn't a table access. A scan op is a
/// full scan (Seq Scan); a seek/lookup reads through an index (Index Scan) whose served columns are
/// its seek keys (`RangeColumns`, not the compared values in `RangeExpressions`).
fn mssql_reloop_node(reloop: roxmltree::Node) -> Option<PlanNode> {
    let physical = reloop.attribute("PhysicalOp")?;
    let seek = physical.contains("Seek") || physical == "Key Lookup" || physical == "RID Lookup";
    let scan = matches!(
        physical,
        "Table Scan" | "Clustered Index Scan" | "Index Scan"
    );
    if !seek && !scan {
        return None;
    }
    let (table, index) = mssql_object(reloop)?;
    let access = if scan {
        Access::SeqScan
    } else {
        Access::IndexScan {
            index: index.map(|s| str_name(&s)),
        }
    };
    let index_keys = if seek {
        mssql_seek_columns(reloop)
    } else {
        Vec::new()
    };
    Some(scan_leaf(access, Some(str_name(&table)), index_keys))
}

/// The `(table, index)` of the `<Object>` for this `RelOp` — the first one found without
/// descending into a *nested* `RelOp` (a different table access). Names lose SQL Server's `[ ]`.
fn mssql_object(reloop: roxmltree::Node) -> Option<(String, Option<String>)> {
    fn find<'a>(node: roxmltree::Node<'a, 'a>) -> Option<roxmltree::Node<'a, 'a>> {
        for child in node.children().filter(roxmltree::Node::is_element) {
            if child.has_tag_name("RelOp") {
                continue;
            }
            if child.has_tag_name("Object") && child.attribute("Table").is_some() {
                return Some(child);
            }
            if let Some(found) = find(child) {
                return Some(found);
            }
        }
        None
    }
    let obj = find(reloop)?;
    Some((
        mssql_strip_brackets(obj.attribute("Table")?),
        obj.attribute("Index").map(mssql_strip_brackets),
    ))
}

/// The seek-key columns of a `RelOp`: the `<ColumnReference>`s under `<RangeColumns>` (the index
/// keys), deduped. `RangeExpressions` (the compared values, possibly another table's columns) are
/// excluded. A seek `<RelOp>` is a leaf, so its `RangeColumns` all belong to it.
fn mssql_seek_columns(reloop: roxmltree::Node) -> Vec<Name> {
    let mut out: Vec<Name> = reloop
        .descendants()
        .filter(|n| n.has_tag_name("RangeColumns"))
        .flat_map(|rc| {
            rc.descendants()
                .filter(|n| n.has_tag_name("ColumnReference"))
        })
        .filter_map(|c| c.attribute("Column"))
        .map(|s| str_name(&mssql_strip_brackets(s)))
        .collect();
    out.dedup_by(|a, b| a.normalized() == b.normalized());
    out
}

fn mssql_strip_brackets(s: &str) -> String {
    s.trim_start_matches('[').trim_end_matches(']').to_string()
}

/// The column references in a plan condition on JSON field `key` (Postgres `Index Cond`/`Filter`).
fn cond_columns(v: &Value, key: &str, dialect: Dialect) -> Vec<Name> {
    str_field(v, key)
        .map(|c| cond_columns_str(&c, dialect))
        .unwrap_or_default()
}

/// The column references in a plan condition string, e.g. `"(status = 'active'::text)"` (PG) or
/// ``"(`t`.`status` = 'active')"`` (MySQL). Parsed as an expression via the real grammar, deduped.
/// Anything that doesn't parse yields no columns — the node simply gives no signal.
fn cond_columns_str(cond: &str, dialect: Dialect) -> Vec<Name> {
    let Ok(stmts) = parse(&format!("SELECT 1 WHERE {cond}"), dialect) else {
        return Vec::new();
    };
    let mut out: Vec<Name> = Vec::new();
    for stmt in &stmts {
        if let Analyzed::Query(q) = translate(stmt) {
            if let Relation::Stage(s) = &q.body {
                for e in &s.filter {
                    collect_cols(e, &mut out);
                }
            }
        }
    }
    out.dedup_by(|a, b| a.normalized() == b.normalized());
    out
}

fn collect_cols(e: &Expr, out: &mut Vec<Name>) {
    match e {
        Expr::Column(c) => out.push(c.name.clone()),
        Expr::Binary { left, right, .. } => {
            collect_cols(left, out);
            collect_cols(right, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } | Expr::InSubquery { expr, .. } => {
            collect_cols(expr, out)
        }
        Expr::Function { args, .. } => args.iter().for_each(|a| collect_cols(a, out)),
        Expr::Case {
            operand,
            whens,
            else_branch,
            ..
        } => {
            if let Some(o) = operand {
                collect_cols(o, out);
            }
            for (w, t) in whens {
                collect_cols(w, out);
                collect_cols(t, out);
            }
            if let Some(b) = else_branch {
                collect_cols(b, out);
            }
        }
        Expr::InList { expr, list, .. } => {
            collect_cols(expr, out);
            list.iter().for_each(|i| collect_cols(i, out));
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_cols(expr, out);
            collect_cols(low, out);
            collect_cols(high, out);
        }
        _ => {}
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

/// A plan identifier. Postgres renders unquoted names folded to lowercase, which is exactly the
/// normalized form the stage tree matches on; quoted-identifier case is best-effort.
fn name(s: String) -> Name {
    Name::new(s, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(json: &str) -> Plan {
        Plan::from_pg_explain_json(json, Dialect::Postgres).unwrap()
    }

    fn cols(ns: &[Name]) -> Vec<String> {
        ns.iter().map(Name::normalized).collect()
    }

    #[test]
    fn seq_scan_captures_filtered_column_and_rows() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"users","Alias":"u",
            "Filter":"(status = 'active'::text)","Plan Rows":100}}]"#,
        );
        assert!(matches!(p.root.access, Access::SeqScan));
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "users");
        assert_eq!(p.root.alias.as_ref().unwrap().normalized(), "u");
        assert_eq!(cols(&p.root.filtered), ["status"]);
        assert!(p.root.index_keys.is_empty());
        assert_eq!(p.root.est_rows, Some(100));
        assert!(!p.analyzed);
    }

    #[test]
    fn index_scan_captures_index_and_served_column() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"users",
            "Index Name":"users_email_idx","Index Cond":"(email = 'x'::text)"}}]"#,
        );
        match &p.root.access {
            Access::IndexScan { index } => {
                assert_eq!(index.as_ref().unwrap().normalized(), "users_email_idx")
            }
            other => panic!("expected index scan, got {other:?}"),
        }
        assert_eq!(cols(&p.root.index_keys), ["email"]);
    }

    #[test]
    fn analyze_sets_actual_rows() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"orders",
            "Filter":"(total > 100)","Actual Rows":4200000}}]"#,
        );
        assert!(p.analyzed);
        assert_eq!(p.root.actual_rows, Some(4200000));
        assert_eq!(cols(&p.root.filtered), ["total"]); // compound `orders.total` would also reduce to `total`
    }

    #[test]
    fn bitmap_heap_scan_folds_its_index_child() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Bitmap Heap Scan","Relation Name":"orders",
            "Recheck Cond":"(user_id = 5)","Plans":[
              {"Node Type":"Bitmap Index Scan","Index Name":"orders_uid_idx",
               "Index Cond":"(user_id = 5)"}]}}]"#,
        );
        match &p.root.access {
            Access::IndexScan { index } => {
                assert_eq!(index.as_ref().unwrap().normalized(), "orders_uid_idx")
            }
            other => panic!("expected folded index scan, got {other:?}"),
        }
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(cols(&p.root.index_keys), ["user_id"]);
    }

    #[test]
    fn walks_both_relations_under_a_join() {
        let p = pg(r#"[{"Plan":{"Node Type":"Hash Join","Plans":[
            {"Node Type":"Seq Scan","Relation Name":"orders"},
            {"Node Type":"Seq Scan","Relation Name":"users"}]}}]"#);
        let scanned: Vec<String> = p
            .nodes()
            .iter()
            .filter_map(|n| n.relation.as_ref().map(Name::normalized))
            .collect();
        assert_eq!(scanned, ["orders", "users"]);
        assert!(matches!(&p.root.access, Access::Other(s) if s == "Hash Join"));
    }

    #[test]
    fn verdict_suppresses_when_index_serves_the_column() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"users",
            "Index Name":"users_email_idx","Index Cond":"(email = 'x'::text)"}}]"#,
        );
        assert_eq!(p.verdict("users", None, "email"), Verdict::Suppress);
    }

    #[test]
    fn verdict_confirms_a_seq_scan_with_rows() {
        let p = pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"users",
            "Filter":"(status = 'active'::text)","Actual Rows":5000000}}]"#);
        assert_eq!(
            p.verdict("users", None, "status"),
            Verdict::Confirm {
                actual_rows: Some(5000000)
            }
        );
    }

    #[test]
    fn verdict_no_signal_when_index_serves_a_different_column() {
        // Index scan on email; the finding is about `status`, which the index doesn't serve.
        let p = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"users",
            "Index Name":"users_email_idx","Index Cond":"(email = 'x'::text)"}}]"#,
        );
        assert_eq!(p.verdict("users", None, "status"), Verdict::NoSignal);
    }

    #[test]
    fn verdict_no_signal_on_self_join_conflict() {
        // Same relation scanned two ways — can't tell which occurrence the finding is about.
        let p = pg(r#"[{"Plan":{"Node Type":"Nested Loop","Plans":[
            {"Node Type":"Seq Scan","Relation Name":"orders","Filter":"(id = 1)"},
            {"Node Type":"Index Scan","Relation Name":"orders","Index Name":"orders_pkey",
             "Index Cond":"(id = 2)"}]}}]"#);
        assert_eq!(p.verdict("orders", None, "id"), Verdict::NoSignal);
    }

    fn mysql(json: &str) -> Plan {
        Plan::from_mysql_explain_json(json, Dialect::Mysql).unwrap()
    }

    #[test]
    fn mysql_full_scan_confirms_from_attached_condition() {
        let p = mysql(
            r#"{"query_block":{"select_id":1,"table":{"table_name":"users","access_type":"ALL",
                "attached_condition":"(`users`.`status` = 'active')"}}}"#,
        );
        assert_eq!(
            p.verdict("users", None, "status"),
            Verdict::Confirm { actual_rows: None }
        );
    }

    #[test]
    fn mysql_index_seek_suppresses_via_used_key_parts() {
        let p = mysql(
            r#"{"query_block":{"select_id":1,"table":{"table_name":"users","access_type":"ref",
                "key":"users_email_idx","used_key_parts":["email"]}}}"#,
        );
        assert_eq!(p.verdict("users", None, "email"), Verdict::Suppress);
    }

    #[test]
    fn mysql_full_index_scan_does_not_suppress() {
        // `access_type: index` scans the whole index (no seek) — `used_key_parts` must NOT count as
        // an index serving the column, or a real full scan would be wrongly suppressed.
        let p = mysql(
            r#"{"query_block":{"select_id":1,"table":{"table_name":"t","access_type":"index",
                "key":"t_a","used_key_parts":["a"],"attached_condition":"(`t`.`a` = 1)"}}}"#,
        );
        assert_eq!(
            p.verdict("t", None, "a"),
            Verdict::Confirm { actual_rows: None }
        );
    }

    #[test]
    fn verdict_matches_a_mysql_alias_node() {
        // MySQL reports the alias as its relation; the verdict is called with (base, Some(alias)).
        let p = mysql(
            r#"{"query_block":{"table":{"table_name":"u","access_type":"ref","key":"ix",
                "used_key_parts":["email"]}}}"#,
        );
        assert_eq!(p.verdict("users", Some("u"), "email"), Verdict::Suppress);
        // Without the alias hint the base table doesn't match the alias-named node → no signal.
        assert_eq!(p.verdict("users", None, "email"), Verdict::NoSignal);
    }

    fn sqlite(json: &str) -> Plan {
        Plan::from_sqlite_query_plan(json, Dialect::Sqlite).unwrap()
    }

    #[test]
    fn sqlite_search_seek_suppresses_via_paren_columns() {
        let p = sqlite(
            r#"[{"id":3,"parent":0,"notused":0,
                "detail":"SEARCH users USING INDEX ix_email (email=?)"}]"#,
        );
        assert_eq!(p.verdict("users", None, "email"), Verdict::Suppress);
        // A different column on the same seek isn't served → unchanged.
        assert_eq!(p.verdict("users", None, "name"), Verdict::NoSignal);
    }

    #[test]
    fn sqlite_scan_confirms() {
        let p = sqlite(r#"[{"id":2,"parent":0,"notused":0,"detail":"SCAN orders"}]"#);
        assert_eq!(
            p.verdict("orders", None, "status"),
            Verdict::Confirm { actual_rows: None }
        );
    }

    #[test]
    fn sqlite_full_covering_index_scan_is_not_a_seek() {
        // Reported as SCAN, not SEARCH → a Seq Scan, so it confirms rather than suppresses.
        let p = sqlite(r#"[{"detail":"SCAN t USING COVERING INDEX t_a"}]"#);
        assert_eq!(
            p.verdict("t", None, "a"),
            Verdict::Confirm { actual_rows: None }
        );
    }

    /// Wrap `<RelOp>` fragments in the namespaced SHOWPLAN envelope, like real SQL Server output.
    fn showplan(reloops: &str) -> Plan {
        let xml = format!(
            r#"<?xml version="1.0"?>
            <ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan">
              <BatchSequence><Batch><Statements><StmtSimple><QueryPlan>{reloops}</QueryPlan>
              </StmtSimple></Statements></Batch></BatchSequence>
            </ShowPlanXML>"#
        );
        Plan::from_mssql_showplan_xml(&xml, Dialect::Mssql).unwrap()
    }

    #[test]
    fn mssql_seek_suppresses_via_range_columns() {
        let p = showplan(
            r#"<RelOp PhysicalOp="Index Seek">
                 <IndexScan><Object Table="[users]" Index="[ix_email]"/>
                   <SeekPredicates><SeekPredicateNew><SeekKeys><Prefix><RangeColumns>
                     <ColumnReference Table="[users]" Column="[email]"/>
                   </RangeColumns><RangeExpressions>
                     <ColumnReference Table="[other]" Column="[val]"/>
                   </RangeExpressions></Prefix></SeekKeys></SeekPredicateNew></SeekPredicates>
                 </IndexScan></RelOp>"#,
        );
        // `email` (a RangeColumn) is served → Suppress; `val` (a RangeExpression) isn't in index_keys.
        assert_eq!(p.verdict("users", None, "email"), Verdict::Suppress);
        assert_eq!(p.verdict("users", None, "val"), Verdict::NoSignal);
    }

    #[test]
    fn mssql_table_scan_confirms() {
        let p = showplan(
            r#"<RelOp PhysicalOp="Table Scan">
                 <TableScan><Object Table="[orders]"/></TableScan></RelOp>"#,
        );
        assert_eq!(
            p.verdict("orders", None, "status"),
            Verdict::Confirm { actual_rows: None }
        );
    }

    #[test]
    fn mssql_malformed_xml_errors_without_panicking() {
        assert!(matches!(
            Plan::from_mssql_showplan_xml("<not-closed", Dialect::Mssql),
            Err(PlanError::NotXml(_))
        ));
    }

    #[test]
    fn dispatcher_routes_by_dialect() {
        let mysql_json = r#"{"query_block":{"table":{"table_name":"t","access_type":"ALL"}}}"#;
        assert!(Plan::from_explain(mysql_json, Dialect::Mysql).is_ok());
        let sqlite_json = r#"[{"detail":"SCAN t"}]"#;
        assert!(Plan::from_explain(sqlite_json, Dialect::Sqlite).is_ok());
        let mssql_xml = r#"<ShowPlanXML><RelOp PhysicalOp="Table Scan">
            <TableScan><Object Table="[t]"/></TableScan></RelOp></ShowPlanXML>"#;
        assert!(Plan::from_explain(mssql_xml, Dialect::Mssql).is_ok());
    }

    #[test]
    fn rejects_non_plan_json() {
        assert!(matches!(
            Plan::from_pg_explain_json("[]", Dialect::Postgres),
            Err(PlanError::NoPlan)
        ));
        assert!(matches!(
            Plan::from_pg_explain_json("not json", Dialect::Postgres),
            Err(PlanError::NotJson(_))
        ));
    }
}
