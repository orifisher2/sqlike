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
    /// What this node *is* — a scan, join, sort, … See [`NodeKind`].
    pub kind: NodeKind,
    /// How the leaf reads its relation. `Some` only when `kind == Scan`; `None` for every other
    /// node (a join or a sort has no access method).
    pub access: Option<Access>,
    pub relation: Option<Name>,
    pub alias: Option<Name>,
    /// Columns the index served (from `Index Cond`) — index-supported.
    pub index_keys: Vec<Name>,
    /// Columns filtered after the scan (from `Filter`) — not index-supported.
    pub filtered: Vec<Name>,
    pub est_rows: Option<u64>,
    pub actual_rows: Option<u64>,
    /// Estimated total cost (PG `Total Cost`, MSSQL `EstimatedTotalSubtreeCost`). Arbitrary units,
    /// comparable only within one plan.
    pub est_cost: Option<f64>,
    /// Actual wall time for this node across all loops, ms (only with `EXPLAIN ANALYZE`).
    pub actual_time_ms: Option<f64>,
    /// The node spilled to disk (external sort, hash batches, spill warnings).
    pub spilled: bool,
    pub children: Vec<PlanNode>,
}

/// What a plan node is. A `Scan` also carries an [`Access`]; everything else is a shape operator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeKind {
    /// A table access — see the node's `access`.
    Scan,
    NestedLoop,
    HashJoin,
    MergeJoin,
    Sort,
    Aggregate,
    /// A hash-build node feeding a hash join.
    Hash,
    Limit,
    /// A materialize / CTE-scan barrier.
    Materialize,
    /// Any node kind we don't model — the engine's own label, kept so parsing never fails.
    Other(String),
}

/// How a node reads its relation. Only ever carried by a `Scan` node.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Access {
    SeqScan,
    /// A plain, index-only, or bitmap index scan — all "an index served a column".
    IndexScan {
        index: Option<Name>,
    },
}

impl PlanNode {
    /// A cross-dialect heaviness key for ranking hotspots, preferring measured signals: actual
    /// time → estimated cost → actual rows → estimated rows. `0.0` when the node carries none
    /// (e.g. SQLite, which reports shape only). A plan is uniformly analyzed or not, so every node
    /// falls back to the same signal → the ranking is internally consistent.
    pub fn weight(&self) -> f64 {
        self.actual_time_ms
            .or(self.est_cost)
            .or(self.actual_rows.map(|r| r as f64))
            .or(self.est_rows.map(|r| r as f64))
            .unwrap_or(0.0)
    }
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

    /// Parse a MySQL `EXPLAIN` JSON document. Two shapes:
    /// - **v1** `EXPLAIN FORMAT=JSON` (a `query_block`) — the estimated plan. Each table access
    ///   becomes a leaf under a synthetic `Query` root; no actual rows.
    /// - **v2** `EXPLAIN ANALYZE FORMAT=JSON` (8.3+, `explain_json_format_version=2`) — the iterator
    ///   tree keyed by `operation`/`inputs`, carrying actual rows/time. Parsed into a real tree.
    pub fn from_mysql_explain_json(json: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let v: Value = serde_json::from_str(json).map_err(|e| PlanError::NotJson(e.to_string()))?;
        if v.get("query_block").is_some() {
            let mut children = Vec::new();
            collect_mysql_tables(&v, dialect, &mut children);
            return Ok(Plan {
                root: query_root(children),
                analyzed: false,
            });
        }
        if v.get("operation").is_some() {
            let mut analyzed = false;
            let root = parse_mysql_v2(&v, &mut analyzed);
            return Ok(Plan { root, analyzed });
        }
        Err(PlanError::NoPlan)
    }

    /// Parse a SQL Server `SHOWPLAN_XML` (estimated) or `STATISTICS XML` (actual) document into the
    /// real `<RelOp>` tree. Each op's `PhysicalOp` maps to a [`NodeKind`]; a scan/seek also carries
    /// its [`Access`] + served columns. `EstimatedTotalSubtreeCost` → cost; `<RunTimeInformation>`
    /// (present only for STATISTICS XML) → actual rows/time, marking the plan analyzed.
    pub fn from_mssql_showplan_xml(xml: &str, _dialect: Dialect) -> Result<Plan, PlanError> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| PlanError::NotXml(e.to_string()))?;
        let roots: Vec<roxmltree::Node> = doc
            .descendants()
            .filter(|n| n.has_tag_name("QueryPlan"))
            .filter_map(|qp| qp.children().find(|c| c.has_tag_name("RelOp")))
            .collect();
        // Fall back to any top-level RelOp (test fragments may omit the QueryPlan wrapper's parent).
        let roots = if roots.is_empty() {
            doc.descendants()
                .filter(|n| n.has_tag_name("RelOp"))
                .filter(|n| !n.ancestors().skip(1).any(|a| a.has_tag_name("RelOp")))
                .collect()
        } else {
            roots
        };
        let mut analyzed = false;
        let mut children: Vec<PlanNode> = roots
            .iter()
            .map(|r| mssql_node(*r, &mut analyzed))
            .collect();
        let root = if children.len() == 1 {
            children.pop().unwrap()
        } else {
            query_root(children)
        };
        Ok(Plan { root, analyzed })
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
            .any(|n| matches!(n.access, Some(Access::IndexScan { .. })) && has(&n.index_keys));
        let scanned = on_table
            .iter()
            .find(|n| matches!(n.access, Some(Access::SeqScan)) || has(&n.filtered));
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

    let (kind, access) = classify(&node_type, v);
    let mut node = PlanNode {
        kind,
        access,
        relation: str_field(v, "Relation Name").map(name),
        alias: str_field(v, "Alias").map(name),
        index_keys: cond_columns(v, "Index Cond", dialect),
        filtered: cond_columns(v, "Filter", dialect),
        est_rows: v.get("Plan Rows").and_then(Value::as_u64),
        actual_rows,
        est_cost: v.get("Total Cost").and_then(Value::as_f64),
        // PG reports per-loop time; the true node cost is time × loops.
        actual_time_ms: pg_actual_time(v),
        spilled: pg_spilled(v),
        children,
    };

    // A Bitmap Heap Scan carries the table; its child Bitmap Index Scan carries the index and the
    // condition. Fold the child up so the node reads as one index scan on the relation.
    if node_type == "Bitmap Heap Scan" {
        if let Some(idx) = node
            .children
            .iter()
            .find(|c| matches!(c.access, Some(Access::IndexScan { .. })))
        {
            node.access = idx.access.clone();
            node.index_keys = idx.index_keys.clone();
        }
    }

    node
}

/// Total actual node time (ms) = PG's per-loop `Actual Total Time` × `Actual Loops`.
fn pg_actual_time(v: &Value) -> Option<f64> {
    let per_loop = v.get("Actual Total Time").and_then(Value::as_f64)?;
    let loops = v.get("Actual Loops").and_then(Value::as_f64).unwrap_or(1.0);
    Some(per_loop * loops)
}

/// A PG node spilled to disk: an external-merge sort, or a hash node that used disk batches.
fn pg_spilled(v: &Value) -> bool {
    str_field(v, "Sort Method").is_some_and(|m| m.contains("external"))
        || v.get("Disk Usage")
            .and_then(Value::as_u64)
            .is_some_and(|d| d > 0)
}

/// A Postgres node type → its [`NodeKind`] and, for a scan, its [`Access`].
fn classify(node_type: &str, v: &Value) -> (NodeKind, Option<Access>) {
    let index = || Access::IndexScan {
        index: str_field(v, "Index Name").map(name),
    };
    match node_type {
        "Seq Scan" => (NodeKind::Scan, Some(Access::SeqScan)),
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" | "Bitmap Heap Scan" => {
            (NodeKind::Scan, Some(index()))
        }
        "Nested Loop" => (NodeKind::NestedLoop, None),
        "Hash Join" => (NodeKind::HashJoin, None),
        "Merge Join" => (NodeKind::MergeJoin, None),
        "Sort" | "Incremental Sort" => (NodeKind::Sort, None),
        "Aggregate" | "GroupAggregate" | "HashAggregate" => (NodeKind::Aggregate, None),
        "Hash" => (NodeKind::Hash, None),
        "Limit" => (NodeKind::Limit, None),
        "Materialize" | "CTE Scan" => (NodeKind::Materialize, None),
        other => (NodeKind::Other(other.to_string()), None),
    }
}

/// A synthetic non-scan root holding all of a plan's table-access leaves.
fn query_root(children: Vec<PlanNode>) -> PlanNode {
    PlanNode {
        kind: NodeKind::Other("Query".to_string()),
        access: None,
        relation: None,
        alias: None,
        index_keys: Vec::new(),
        filtered: Vec::new(),
        est_rows: None,
        actual_rows: None,
        est_cost: None,
        actual_time_ms: None,
        spilled: false,
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
        kind: NodeKind::Scan,
        access: Some(access),
        // MySQL reports the alias here when one exists; there's no separate base-table field.
        relation: str_field(v, "table_name").map(name),
        alias: None,
        index_keys,
        filtered,
        est_rows: v
            .get("cost_info")
            .and_then(|c| c.get("prefix_rows"))
            .and_then(Value::as_u64),
        actual_rows: None,
        est_cost: v
            .get("cost_info")
            .and_then(|c| c.get("read_cost"))
            .and_then(json_f64),
        actual_time_ms: None,
        spilled: false,
        children: Vec::new(),
    }
}

/// A numeric JSON value that may be a bare number or a string (MySQL renders `cost_info` costs as
/// quoted strings, e.g. `"read_cost": "1.00"`).
fn json_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn str_name(s: &str) -> Name {
    Name::new(s.to_string(), false)
}

/// Parse a node of the MySQL v2 iterator tree (`EXPLAIN ANALYZE FORMAT=JSON`), recursing over
/// `inputs`. Carries actual rows/time and estimated cost; index-served columns aren't extracted
/// (the v2 plan drives hotspots/actuals, not the missing-index verdict — that uses the v1/PG path).
fn parse_mysql_v2(v: &Value, analyzed: &mut bool) -> PlanNode {
    let children: Vec<PlanNode> = v
        .get("inputs")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|c| parse_mysql_v2(c, analyzed)).collect())
        .unwrap_or_default();

    let access_type = str_field(v, "access_type").unwrap_or_default();
    let actual_rows = v.get("actual_rows").and_then(Value::as_f64);
    if actual_rows.is_some() {
        *analyzed = true;
    }
    let (kind, access) = mysql_v2_kind(v, &access_type);
    PlanNode {
        kind,
        access,
        relation: str_field(v, "table_name").map(name),
        alias: str_field(v, "alias").map(name),
        index_keys: Vec::new(),
        filtered: Vec::new(),
        est_rows: v
            .get("estimated_rows")
            .and_then(Value::as_f64)
            .map(round_u64),
        actual_rows: actual_rows.map(round_u64),
        est_cost: v.get("estimated_total_cost").and_then(json_f64),
        // v2 reports per-loop `actual_last_row_ms`; the node's total time is that × loops.
        actual_time_ms: mysql_v2_time(v),
        spilled: false,
        children,
    }
}

/// A MySQL v2 node's `access_type` (refined by `index_access_type` / `join_algorithm`) → its kind
/// and, for a table access, its [`Access`].
fn mysql_v2_kind(v: &Value, access_type: &str) -> (NodeKind, Option<Access>) {
    match access_type {
        "table" => (NodeKind::Scan, Some(Access::SeqScan)),
        "index" => {
            // A full index scan serves no seek; a lookup/ref/range does.
            let full = str_field(v, "index_access_type").as_deref() == Some("index_scan");
            let access = if full {
                Access::SeqScan
            } else {
                Access::IndexScan { index: None }
            };
            (NodeKind::Scan, Some(access))
        }
        "join" => {
            let hash = str_field(v, "join_algorithm").as_deref() == Some("hash");
            let kind = if hash {
                NodeKind::HashJoin
            } else {
                NodeKind::NestedLoop
            };
            (kind, None)
        }
        "sort" => (NodeKind::Sort, None),
        "aggregate" | "group_by" => (NodeKind::Aggregate, None),
        "materialized" | "temp_table" => (NodeKind::Materialize, None),
        "limit" => (NodeKind::Limit, None),
        other => (NodeKind::Other(other.to_string()), None),
    }
}

fn mysql_v2_time(v: &Value) -> Option<f64> {
    let last = v.get("actual_last_row_ms").and_then(Value::as_f64)?;
    let loops = v.get("actual_loops").and_then(Value::as_f64).unwrap_or(1.0);
    Some(last * loops)
}

fn round_u64(f: f64) -> u64 {
    f.round().max(0.0) as u64
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
        kind: NodeKind::Scan,
        access: Some(access),
        relation,
        alias: None,
        index_keys,
        filtered: Vec::new(),
        est_rows: None,
        actual_rows: None,
        est_cost: None,
        actual_time_ms: None,
        spilled: false,
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

/// One SQL Server `<RelOp>` → a plan node, recursing over its direct child `<RelOp>`s. A scan/seek
/// op becomes a `Scan` (with `Access` + served columns); every other op maps by `PhysicalOp`.
fn mssql_node(reloop: roxmltree::Node, analyzed: &mut bool) -> PlanNode {
    let physical = reloop.attribute("PhysicalOp").unwrap_or_default();
    let children: Vec<PlanNode> = mssql_child_reloops(reloop)
        .into_iter()
        .map(|r| mssql_node(r, analyzed))
        .collect();
    let (actual_rows, actual_time_ms) = mssql_runtime(reloop);
    if actual_rows.is_some() {
        *analyzed = true;
    }
    let scan = mssql_scan_access(reloop, physical);
    PlanNode {
        kind: if scan.is_some() {
            NodeKind::Scan
        } else {
            mssql_op_kind(physical, reloop.attribute("LogicalOp").unwrap_or_default())
        },
        access: scan.as_ref().map(|s| s.0.clone()),
        relation: scan.as_ref().map(|s| str_name(&s.1)),
        alias: None,
        index_keys: scan.map(|s| s.2).unwrap_or_default(),
        filtered: Vec::new(),
        est_rows: attr_f64(reloop, "EstimateRows").map(round_u64),
        actual_rows,
        est_cost: attr_f64(reloop, "EstimatedTotalSubtreeCost"),
        actual_time_ms,
        spilled: mssql_spilled(reloop),
        children,
    }
}

/// A non-scan `<RelOp>`'s kind from its `PhysicalOp` (a `Hash Match` is a join or an aggregate,
/// disambiguated by `LogicalOp`).
fn mssql_op_kind(physical: &str, logical: &str) -> NodeKind {
    match physical {
        "Sort" => NodeKind::Sort,
        "Nested Loops" => NodeKind::NestedLoop,
        "Merge Join" => NodeKind::MergeJoin,
        "Hash Match" if logical.contains("Aggregate") => NodeKind::Aggregate,
        "Hash Match" => NodeKind::HashJoin,
        "Stream Aggregate" => NodeKind::Aggregate,
        "Top" => NodeKind::Limit,
        "Table Spool" | "Index Spool" => NodeKind::Materialize,
        other => NodeKind::Other(other.to_string()),
    }
}

/// The `(access, table, served-columns)` if this `<RelOp>` is a table access, else `None`. A scan
/// op is a full scan (Seq Scan); a seek/lookup reads through an index (Index Scan) whose served
/// columns are its seek keys (`RangeColumns`, not the `RangeExpressions` compared values).
fn mssql_scan_access(
    reloop: roxmltree::Node,
    physical: &str,
) -> Option<(Access, String, Vec<Name>)> {
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
    Some((access, table, index_keys))
}

/// The `<RelOp>`s directly nested under `reloop` (through its physical-op wrapper elements), not
/// descending past a nested `RelOp` — those are the node's own children.
fn mssql_child_reloops<'a>(reloop: roxmltree::Node<'a, 'a>) -> Vec<roxmltree::Node<'a, 'a>> {
    fn walk<'a>(n: roxmltree::Node<'a, 'a>, out: &mut Vec<roxmltree::Node<'a, 'a>>) {
        for c in n.children().filter(roxmltree::Node::is_element) {
            if c.has_tag_name("RelOp") {
                out.push(c);
            } else {
                walk(c, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(reloop, &mut out);
    out
}

/// This `<RelOp>`'s actuals from its own `<RunTimeInformation>` (STATISTICS XML only), summed over
/// per-thread counters: actual rows and elapsed ms. `(None, None)` for an estimated plan.
fn mssql_runtime(reloop: roxmltree::Node) -> (Option<u64>, Option<f64>) {
    let Some(rti) = reloop
        .children()
        .find(|c| c.has_tag_name("RunTimeInformation"))
    else {
        return (None, None);
    };
    let threads = rti
        .children()
        .filter(|c| c.has_tag_name("RunTimeCountersPerThread"));
    let (mut rows, mut ms, mut any) = (0u64, 0f64, false);
    for t in threads {
        any = true;
        rows += t
            .attribute("ActualRows")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        ms += t
            .attribute("ActualElapsedms")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
    }
    if any {
        (Some(rows), Some(ms))
    } else {
        (None, None)
    }
}

/// A node spilled to `tempdb` (its `<Warnings>` carry a `<SpillToTempDb>`).
fn mssql_spilled(reloop: roxmltree::Node) -> bool {
    reloop.children().any(|c| {
        c.has_tag_name("Warnings") && c.children().any(|w| w.has_tag_name("SpillToTempDb"))
    })
}

fn attr_f64(n: roxmltree::Node, key: &str) -> Option<f64> {
    n.attribute(key).and_then(|s| s.parse().ok())
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
        assert!(matches!(p.root.access, Some(Access::SeqScan)));
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
            Some(Access::IndexScan { index }) => {
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
            Some(Access::IndexScan { index }) => {
                assert_eq!(index.as_ref().unwrap().normalized(), "orders_uid_idx")
            }
            other => panic!("expected folded index scan, got {other:?}"),
        }
        assert_eq!(p.root.kind, NodeKind::Scan);
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
        assert_eq!(p.root.kind, NodeKind::HashJoin);
        assert!(p.root.access.is_none());
    }

    #[test]
    fn pg_captures_kind_cost_time_and_spill() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Sort","Total Cost":8000.5,"Plan Rows":900,
              "Actual Total Time":12.5,"Actual Loops":4,"Actual Rows":900,
              "Sort Method":"external merge","Disk Usage":2048,"Plans":[
                {"Node Type":"Seq Scan","Relation Name":"t","Total Cost":300.0,
                 "Actual Total Time":5.0,"Actual Loops":1,"Actual Rows":900}]}}]"#,
        );
        assert_eq!(p.root.kind, NodeKind::Sort);
        assert!(p.root.access.is_none());
        assert_eq!(p.root.est_cost, Some(8000.5));
        assert_eq!(p.root.actual_time_ms, Some(50.0)); // 12.5ms × 4 loops
        assert!(p.root.spilled);
        assert!(p.analyzed);
        // weight prefers actual time over the cheaper child's cost.
        assert!(p.root.weight() > p.root.children[0].weight());
        assert_eq!(p.root.children[0].kind, NodeKind::Scan);
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

    #[test]
    fn mysql_v2_analyze_builds_tree_with_kinds_rows_time() {
        // Trimmed real `EXPLAIN ANALYZE FORMAT=JSON` (explain_json_format_version=2) output.
        let p = mysql(
            r#"{"operation":"Sort: o.total","access_type":"sort","actual_rows":3.0,
              "actual_loops":1,"actual_last_row_ms":0.888,"inputs":[
                {"operation":"Inner hash join","access_type":"join","join_algorithm":"hash",
                 "actual_rows":3.0,"actual_loops":1,"actual_last_row_ms":0.6,
                 "estimated_total_cost":1.2,"inputs":[
                   {"operation":"Table scan on o","access_type":"table","table_name":"orders",
                    "alias":"o","actual_rows":4.0,"actual_loops":1,"actual_last_row_ms":0.007,
                    "estimated_rows":4.0,"estimated_total_cost":0.35}]}]}"#,
        );
        assert!(p.analyzed);
        assert_eq!(p.root.kind, NodeKind::Sort);
        assert_eq!(p.root.actual_rows, Some(3));
        assert_eq!(p.root.actual_time_ms, Some(0.888));
        let join = &p.root.children[0];
        assert_eq!(join.kind, NodeKind::HashJoin);
        assert!(join.access.is_none());
        let scan = &join.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert!(matches!(scan.access, Some(Access::SeqScan)));
        assert_eq!(scan.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(scan.actual_rows, Some(4));
        assert_eq!(scan.est_cost, Some(0.35));
        // Heaviest node by actual time is the sort root.
        assert!(p.root.weight() > join.weight() && join.weight() > scan.weight());
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
    fn mssql_statistics_xml_builds_tree_with_kinds_and_actuals() {
        // Trimmed real `SET STATISTICS XML ON` output: Sort → Nested Loops → Clustered Index Scan.
        let p = showplan(
            r#"<RelOp PhysicalOp="Sort" LogicalOp="Sort" EstimatedTotalSubtreeCost="0.0184"
                 EstimateRows="2.66">
                 <RunTimeInformation><RunTimeCountersPerThread ActualRows="3"
                   ActualElapsedms="7" ActualExecutions="1"/></RunTimeInformation>
                 <Sort><RelOp PhysicalOp="Nested Loops" LogicalOp="Inner Join"
                     EstimatedTotalSubtreeCost="0.0070" EstimateRows="2.66">
                     <RunTimeInformation><RunTimeCountersPerThread ActualRows="3"
                       ActualElapsedms="4"/></RunTimeInformation>
                     <NestedLoops><RelOp PhysicalOp="Clustered Index Scan"
                         EstimatedTotalSubtreeCost="0.0032" EstimateRows="4">
                         <RunTimeInformation><RunTimeCountersPerThread ActualRows="4"
                           ActualElapsedms="1"/></RunTimeInformation>
                         <IndexScan><Object Table="[orders]" Index="[PK_orders]"/></IndexScan>
                       </RelOp></NestedLoops>
                   </RelOp></Sort></RelOp>"#,
        );
        assert!(p.analyzed);
        assert_eq!(p.root.kind, NodeKind::Sort);
        assert_eq!(p.root.actual_rows, Some(3));
        assert_eq!(p.root.actual_time_ms, Some(7.0));
        assert_eq!(p.root.est_cost, Some(0.0184));
        let join = &p.root.children[0];
        assert_eq!(join.kind, NodeKind::NestedLoop);
        let scan = &join.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert!(matches!(scan.access, Some(Access::SeqScan)));
        assert_eq!(scan.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(scan.actual_rows, Some(4));
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
