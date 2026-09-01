//! Query-plan model + a Postgres `EXPLAIN (FORMAT JSON)` parser.
//!
//! Pure — JSON text in, a normalized [`Plan`] out, no DB or I/O — so it sits beside `schema`
//! and `tokenize`, and the CLI and the engine consume the same type. Per scan node it carries the
//! relation, how it was read, and which columns the index *served* vs. merely *filtered* (for the
//! `unindexed-*` verdict), plus node kind and rows/cost/time so any performance finding can be
//! re-scored by what the plan actually did (see `table_rows`).
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
    /// Actual rows the node produced. PG/MySQL report this **per loop** (an average over
    /// [`loops`](Self::loops)); MariaDB already folds loops in (`r_rows × r_loops`), so its `loops`
    /// stays `None`. Total rows produced is therefore `actual_rows × loops` — see `rows_processed`.
    pub actual_rows: Option<u64>,
    /// How many times this node was executed — the inner side of a nested loop runs once per outer
    /// row, so `actual_rows`/`rows_removed` (both per-loop) must be scaled by this for the true cost.
    /// `None` when the plan doesn't report it (estimated plans, MariaDB's already-folded totals).
    pub loops: Option<u64>,
    /// Estimated total cost (PG `Total Cost`, MSSQL `EstimatedTotalSubtreeCost`). Arbitrary units,
    /// comparable only within one plan.
    pub est_cost: Option<f64>,
    /// Actual wall time for this node across all loops, ms (only with `EXPLAIN ANALYZE`).
    pub actual_time_ms: Option<f64>,
    /// The node spilled to disk (external sort, hash batches, spill warnings).
    pub spilled: bool,
    /// Rows the scan read then discarded (PG `Rows Removed by Filter`) — with `EXPLAIN ANALYZE` only.
    /// `rows_removed ≫ actual_rows` on an index scan means the index matched far more than the query
    /// kept: a weak leading column. `None` where the dialect/plan doesn't report it.
    pub rows_removed: Option<u64>,
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

    /// The share of [`weight`](Self::weight) spent in *this* node, children excluded — the key that
    /// ranks hotspots. Postgres/SQL Server report time and cost as subtree totals (a parent always
    /// outweighs its children), so for those additive signals we subtract the children's weight to
    /// recover self-cost; a rows-only fallback plan (MySQL-estimated, SQLite) isn't additive, so the
    /// node's own weight stands. Clamped at `0.0` against rounding.
    fn self_weight(&self) -> f64 {
        let additive = self.actual_time_ms.is_some() || self.est_cost.is_some();
        if additive {
            let children: f64 = self.children.iter().map(PlanNode::weight).sum();
            (self.weight() - children).max(0.0)
        } else {
            self.weight()
        }
    }

    /// Why this node is heavy — read from its own shape and measured fields, never a benchmark of
    /// ours. Spill is the sharpest signal, then a blown cardinality, then the access shape.
    fn cause(&self) -> HotspotCause {
        if self.spilled {
            return match self.kind {
                NodeKind::Sort => HotspotCause::SortSpilled,
                _ => HotspotCause::HashSpilled,
            };
        }
        let rows = PlanRows {
            est: self.est_rows,
            actual: self.actual_rows,
        };
        if let Some(factor) = rows.skew_factor() {
            return HotspotCause::EstimateActualSkew { factor };
        }
        match self.kind {
            NodeKind::Scan if matches!(self.access, Some(Access::SeqScan)) => {
                HotspotCause::SeqScanReturningRows
            }
            NodeKind::NestedLoop => HotspotCause::NestedLoopHighRows,
            _ => HotspotCause::Heavy,
        }
    }

    /// Total rows this node touched across every loop: `(actual_rows + rows_removed) × loops`. The
    /// raw per-loop rows hide an inner nested-loop node's real work — 1 row/loop × 200k loops is 200k
    /// index fetches, not "1 row". `0` when the node reports no measured rows.
    fn rows_processed(&self) -> u64 {
        let per_loop = self.actual_rows.unwrap_or(0) + self.rows_removed.unwrap_or(0);
        per_loop.saturating_mul(self.loops.unwrap_or(1))
    }

    /// The node's *health* for the diagram's good→bad colour — a judgment about the access **shape**,
    /// independent of how big a share of the query it is (that's `self_weight`/the percentage). Keyed
    /// on the rows it actually **processes** (× loops), never the raw per-loop count: a tight seek is
    /// good however heavy, and an index scan that touches 100k rows — one clustered fetch each, or the
    /// same seek re-run 100k times under a nested loop — is a concern however "few rows" it shows.
    fn health(&self) -> NodeHealth {
        if self.spilled {
            return NodeHealth::Spill;
        }
        let rows = PlanRows {
            est: self.est_rows,
            actual: self.actual_rows,
        };
        if rows.skew_factor().is_some() {
            return NodeHealth::Skew;
        }
        let kept = self.actual_rows.unwrap_or(0);
        let removed = self.rows_removed.unwrap_or(0);
        let loops = self.loops.unwrap_or(1);
        let scanned = self.rows_processed();
        // Reads far more than it keeps → an index would help (missing index / weak leading column).
        let discard_heavy = removed.saturating_mul(loops) >= 1_000 && removed >= 10 * kept.max(1);
        // A node with no measured rows/time carries no signal (a shape-only EXPLAIN) → neutral.
        // Every measured node otherwise gets a colour: green (fine) by default, escalating by shape.
        let measured = self.actual_rows.is_some()
            || self.actual_time_ms.is_some()
            || self.est_cost.is_some()
            || self.est_rows.is_some();
        if !measured {
            return NodeHealth::Neutral;
        }
        match (&self.kind, &self.access) {
            (NodeKind::Scan, access) => {
                let is_index = matches!(access, Some(Access::IndexScan { .. }));
                // Bad: wastes most of what it reads, or a full scan of a big table (an index would
                // help). Attention: reads a lot (a wide index scan = many fetches). Else: fine.
                if discard_heavy || (!is_index && scanned >= SEQ_SCAN_RED) {
                    NodeHealth::InefficientScan
                } else if scanned >= SCAN_YELLOW {
                    NodeHealth::LargeScan
                } else {
                    NodeHealth::Efficient
                }
            }
            (NodeKind::NestedLoop, _) if scanned >= NESTED_LOOP_ROWS => NodeHealth::NestedLoop,
            // Every other measured operator (join, sort, aggregate, gather…) is a fine step.
            _ => NodeHealth::Efficient,
        }
    }

    fn to_hotspot(&self, under_parallel: bool) -> Hotspot {
        Hotspot {
            kind: self.kind.clone(),
            relation: self.relation.as_ref().map(Name::normalized),
            rows: PlanRows {
                est: self.est_rows,
                actual: self.actual_rows,
            },
            cost: self.est_cost,
            time_ms: self.actual_time_ms,
            spilled: self.spilled,
            cause: self.cause(),
            // A parallel worker's node time sums that worker's effort; across N workers it exceeds
            // wall clock, so a bare number misleads anyone comparing it to the query's runtime.
            worker_summed_time: under_parallel && self.actual_time_ms.is_some(),
            linked_rules: Vec::new(),
        }
    }

    /// A `Gather` / `Gather Merge` — its subtree executes across parallel workers, so those nodes'
    /// `Actual Total Time` sums worker effort rather than measuring wall clock.
    fn spawns_parallel_workers(&self) -> bool {
        matches!(&self.kind, NodeKind::Other(s) if s.starts_with("Gather"))
    }

    /// A human label for the node — the access shape for a scan (Seq Scan / Index Scan), else the
    /// kind. What the diagram box shows as its title.
    fn display_label(&self) -> String {
        match (&self.kind, &self.access) {
            (NodeKind::Scan, Some(Access::SeqScan)) => "Seq Scan".into(),
            (NodeKind::Scan, Some(Access::IndexScan { .. })) => "Index Scan".into(),
            (NodeKind::Scan, None) => "Scan".into(),
            (NodeKind::NestedLoop, _) => "Nested Loop".into(),
            (NodeKind::HashJoin, _) => "Hash Join".into(),
            (NodeKind::MergeJoin, _) => "Merge Join".into(),
            (NodeKind::Sort, _) => "Sort".into(),
            (NodeKind::Aggregate, _) => "Aggregate".into(),
            (NodeKind::Hash, _) => "Hash".into(),
            (NodeKind::Limit, _) => "Limit".into(),
            (NodeKind::Materialize, _) => "Materialize".into(),
            (NodeKind::Other(s), _) => s.clone(),
        }
    }

    /// This node as a [`DiagramNode`], recursively — the client-facing projection.
    fn to_diagram(&self) -> DiagramNode {
        self.to_diagram_ctx(false)
    }

    fn to_diagram_ctx(&self, under_parallel: bool) -> DiagramNode {
        let index = match &self.access {
            Some(Access::IndexScan { index }) => index.as_ref().map(Name::normalized),
            _ => None,
        };
        let child_parallel = under_parallel || self.spawns_parallel_workers();
        DiagramNode {
            label: self.display_label(),
            relation: self.relation.as_ref().map(Name::normalized),
            index,
            est_rows: self.est_rows,
            actual_rows: self.actual_rows,
            // Wall-clock-fair (parallel-aware): a parallel worker's time is per-loop, not worker-summed.
            self_weight: self.diagram_self_weight(under_parallel),
            // Own (exclusive) time/cost so the shown numbers agree with the %.
            self_time_ms: self.self_time_ms(under_parallel),
            incl_time_ms: self.incl_time_ms(under_parallel),
            self_cost: self.self_cost(),
            rows_removed: self.rows_removed,
            // The columns the node keys on — index condition and post-scan filter.
            index_cols: self.index_keys.iter().map(Name::normalized).collect(),
            filter_cols: self.filtered.iter().map(Name::normalized).collect(),
            spilled: self.spilled,
            skew_factor: PlanRows {
                est: self.est_rows,
                actual: self.actual_rows,
            }
            .skew_factor(),
            health: self.health(),
            // Surfaced only when it changes the story — an inner node re-run many times.
            loops: self.loops.filter(|&l| l > 1),
            children: self
                .children
                .iter()
                .map(|c| c.to_diagram_ctx(child_parallel))
                .collect(),
        }
    }

    /// Wall-clock-fair inclusive weight for the diagram: a parallel worker's `time × loops` is
    /// worker-summed, so under a `Gather` the per-loop time is its wall-clock share. Without this the
    /// parallel subtree double-counts and the Gather collapses to 0% (percentages don't add up).
    fn diagram_inclusive(&self, under_parallel: bool) -> f64 {
        if under_parallel && self.actual_time_ms.is_some() {
            if let Some(l) = self.loops.filter(|&l| l > 1) {
                return self.weight() / l as f64;
            }
        }
        self.weight()
    }

    /// Like [`self_weight`](Self::self_weight) but parallel-aware (see [`diagram_inclusive`]).
    fn diagram_self_weight(&self, under_parallel: bool) -> f64 {
        let additive = self.actual_time_ms.is_some() || self.est_cost.is_some();
        if additive {
            let child_parallel = under_parallel || self.spawns_parallel_workers();
            let children: f64 = self
                .children
                .iter()
                .map(|c| c.diagram_inclusive(child_parallel))
                .sum();
            (self.diagram_inclusive(under_parallel) - children).max(0.0)
        } else {
            self.weight()
        }
    }

    /// Parallel-aware inclusive time (ms) — a Gather worker's `time × loops` counted per-loop.
    fn incl_time_ms(&self, under_parallel: bool) -> Option<f64> {
        let t = self.actual_time_ms?;
        if under_parallel {
            if let Some(l) = self.loops.filter(|&l| l > 1) {
                return Some(t / l as f64);
            }
        }
        Some(t)
    }

    /// The node's OWN time (ms): inclusive minus children — the value the `%` is built from, so they
    /// agree (a node that's 150ms inclusive but did 8ms itself reads 8ms → 5%, not 150ms → 5%).
    fn self_time_ms(&self, under_parallel: bool) -> Option<f64> {
        let incl = self.incl_time_ms(under_parallel)?;
        let child_parallel = under_parallel || self.spawns_parallel_workers();
        let kids: f64 = self
            .children
            .iter()
            .filter_map(|c| c.incl_time_ms(child_parallel))
            .sum();
        Some((incl - kids).max(0.0))
    }

    /// The node's OWN estimated cost: inclusive minus children (PG cost is cumulative up the tree).
    fn self_cost(&self) -> Option<f64> {
        let incl = self.est_cost?;
        let kids: f64 = self.children.iter().filter_map(|c| c.est_cost).sum();
        Some((incl - kids).max(0.0))
    }
}

/// A full sequential scan reading at least this many rows is a full-table read of a big table — an
/// index would usually help, so it grades red.
const SEQ_SCAN_RED: u64 = 50_000;
/// A scan touching at least this many rows is worth attention (yellow) even if not clearly bad.
const SCAN_YELLOW: u64 = 10_000;
/// A nested loop producing at least this many rows signals a join blow-up.
const NESTED_LOOP_ROWS: u64 = 50_000;

/// A node's health for the plan diagram's good→bad colour — the access shape's quality, **not** its
/// share of runtime (a tight seek is good however heavy, a wasteful scan bad however light).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeHealth {
    /// A plain operator (join, sort, aggregate) or a small scan — nothing to flag.
    Neutral,
    /// A tight index seek returning about what it reads — the ideal access. (good)
    Efficient,
    /// A sequential scan reading a lot of rows — attention. (warn)
    LargeScan,
    /// A scan that discards most of what it reads — likely a missing or unselective index. (bad)
    InefficientScan,
    /// A nested loop producing many rows — a re-scan blow-up. (bad)
    NestedLoop,
    /// Actual rows dwarf the estimate — a planner misestimate. (warn)
    Skew,
    /// Spilled to disk. (bad)
    Spill,
}

/// The client-facing projection of a [`PlanNode`] for the web plan diagram: display-ready, with the
/// `self_weight` the hotspots rank by precomputed here (so the diagram's heat can't drift from a JS
/// re-derivation) plus the skew/spill flags the badges need. Deliberately *not* the raw model — it
/// carries computed values the serialized [`Plan`] does not, and it shields the diagram from model
/// churn. A future CLI ASCII renderer walks this same tree.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiagramNode {
    /// The node's display title (e.g. "Seq Scan", "Hash Join").
    pub label: String,
    pub relation: Option<String>,
    /// The index this scan used, when it read through one.
    pub index: Option<String>,
    pub est_rows: Option<u64>,
    pub actual_rows: Option<u64>,
    /// The node's own cost share — the heat key. Parallel-aware (a Gather's workers count per-loop).
    pub self_weight: f64,
    /// The node's OWN time, ms (inclusive minus children, parallel-aware) — matches `self_weight`/the
    /// `%`, so the shown time and the share never contradict. `None` without ANALYZE.
    pub self_time_ms: Option<f64>,
    /// The node's INCLUSIVE time, ms (this node + its whole subtree, parallel-aware) — the "subtree"
    /// figure, so you can see which branch of the plan holds the cost. `None` without ANALYZE.
    pub incl_time_ms: Option<f64>,
    /// The node's OWN estimated cost (inclusive minus children). `None` when the plan has no costs.
    pub self_cost: Option<f64>,
    /// Rows read then discarded by a filter (PG `Rows Removed by Filter`) — surfaced in the detail.
    pub rows_removed: Option<u64>,
    /// Columns the index condition keyed on — the collapsed detail.
    pub index_cols: Vec<String>,
    /// Columns filtered after the scan — the collapsed detail.
    pub filter_cols: Vec<String>,
    pub spilled: bool,
    /// `actual / est` when actual dwarfs the estimate (≥ [`SKEW_FACTOR`]×) — else `None`.
    pub skew_factor: Option<u64>,
    /// The access shape's quality — the diagram's good→bad colour, decoupled from `self_weight`.
    pub health: NodeHealth,
    /// Loop count when the node ran more than once (an inner nested-loop side) — else `None`. The
    /// per-loop rows the diagram shows are × this; the web renders it as a `×N loops` badge.
    pub loops: Option<u64>,
    pub children: Vec<DiagramNode>,
}

/// Flatten the tree for ranking, tagging each node with whether it runs under a parallel `Gather`.
fn collect_ranked<'a>(
    node: &'a PlanNode,
    under_parallel: bool,
    out: &mut Vec<(&'a PlanNode, bool)>,
) {
    out.push((node, under_parallel));
    let children_parallel = under_parallel || node.spawns_parallel_workers();
    for child in &node.children {
        collect_ranked(child, children_parallel, out);
    }
}

/// How many heaviest nodes a plan reports as hotspots.
const HOTSPOT_TOP_N: usize = 5;

/// One heavy node in the plan the caller supplied: what it is, how heavy, and why. A ranked summary
/// view over the plan — the actionable fix lives in the cross-linked findings (`linked_rules`), not
/// here, so there is one voice for "add an index on x".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Hotspot {
    pub kind: NodeKind,
    /// The scanned table's normalized name, when this node is a scan — `None` for a join/sort/etc.
    pub relation: Option<String>,
    pub rows: PlanRows,
    pub cost: Option<f64>,
    pub time_ms: Option<f64>,
    pub spilled: bool,
    pub cause: HotspotCause,
    /// `time_ms` sums the effort of parallel workers (the node ran under a `Gather`), so it exceeds
    /// the query's wall-clock runtime — a front door must say so rather than show a bare figure.
    pub worker_summed_time: bool,
    /// Rule ids of performance findings on this node's relation — the actionable items whose fix
    /// addresses this cost. Filled by the analyzer; empty from the pure [`Plan::hotspots`].
    pub linked_rules: Vec<String>,
}

/// Why a hotspot is heavy. Descriptive: each variant is read from the node's own measured fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HotspotCause {
    /// A sequential scan returning many rows — a candidate index, if the linked finding agrees.
    SeqScanReturningRows,
    /// A nested loop over a large input — a hash join / join index may be cheaper.
    NestedLoopHighRows,
    /// A sort that spilled to disk (external merge) — memory pressure or an avoidable sort.
    SortSpilled,
    /// A hash that spilled to disk (batched) — memory pressure.
    HashSpilled,
    /// Actual rows ran `factor`× past the estimate — the plan is built on bad cardinality.
    EstimateActualSkew { factor: u64 },
    /// The costliest node with no sharper story.
    Heavy,
}

impl fmt::Display for HotspotCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotspotCause::SeqScanReturningRows => {
                write!(f, "sequential scan returning many rows")
            }
            HotspotCause::NestedLoopHighRows => write!(f, "nested loop over a large input"),
            HotspotCause::SortSpilled => write!(f, "sort spilled to disk (external merge)"),
            HotspotCause::HashSpilled => write!(f, "hash spilled to disk"),
            HotspotCause::EstimateActualSkew { factor } => {
                write!(
                    f,
                    "actual rows {factor}\u{00d7} the estimate (stale statistics)"
                )
            }
            HotspotCause::Heavy => write!(f, "the costliest node in this plan"),
        }
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
            // Postgres text plans never begin with `[`/`{`, so the first non-space char picks the
            // format: JSON when the caller added `(FORMAT JSON)`, else the default text plan.
            Dialect::Postgres if text.trim_start().starts_with(['[', '{']) => {
                Self::from_pg_explain_json(text, dialect)
            }
            Dialect::Postgres => Self::from_pg_explain_text(text, dialect),
            // MySQL's `EXPLAIN ANALYZE` default output is a `-> …` iterator tree; both JSON shapes
            // (v1 `query_block`, v2 `operation`) begin with `{`. MariaDB is deliberately not sniffed
            // here — its non-JSON `ANALYZE` is a table, not this tree (`docs/phase-text-t2-mysql-tree.md`).
            Dialect::Mysql if !text.trim_start().starts_with('{') => {
                Self::from_mysql_explain_text(text, dialect)
            }
            Dialect::Mysql => Self::from_mysql_explain_json(text, dialect),
            Dialect::Sqlite => Self::from_sqlite_query_plan(text, dialect),
            Dialect::Mssql => Self::from_mssql_showplan_xml(text, dialect),
            // Measured in MA2 against real MariaDB 11.4 (`mariadb_document_drives_the_same_verdicts`):
            // its `EXPLAIN FORMAT=JSON` uses the same `query_block`/`table`/`access_type` vocabulary,
            // so the MySQL v1 path reads it correctly. MariaDB's executed form is spelled
            // `ANALYZE FORMAT=JSON` and adds `r_*` fields to that same shape — it does not use
            // MySQL's v2 `operation` tree, so it lands on the v1 path too.
            Dialect::Mariadb => Self::from_mysql_explain_json(text, dialect),
            Dialect::Duckdb => Self::from_duckdb_explain_json(text, dialect),
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

    /// Parse a Postgres **plain-text** `EXPLAIN` / `EXPLAIN ANALYZE` plan into the same [`Plan`] the
    /// JSON parser produces (T1). Handles the raw driver form and the `psql` decoration (a
    /// `QUERY PLAN` header, a `---` rule, a per-line leading space, a trailing `(N rows)`). The tree
    /// is read by relative indentation: a `->` line is a child, a deeper non-`->` line is a detail.
    pub fn from_pg_explain_text(text: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let lines: Vec<PgTextLine> = strip_psql(text)
            .into_iter()
            .filter_map(PgTextLine::parse)
            .collect();
        let mut it = lines.into_iter().peekable();
        // The first line is the root; its indent is the baseline everything else nests under.
        let first = it.peek().ok_or(PlanError::NoPlan)?.indent;
        let mut analyzed = false;
        let root =
            pg_text_subtree(&mut it, first, dialect, &mut analyzed).ok_or(PlanError::NoPlan)?;
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
            let mut analyzed = false;
            collect_mysql_tables(&v, dialect, &mut children, &mut analyzed);
            return Ok(Plan {
                root: query_root(children),
                analyzed,
            });
        }
        if v.get("operation").is_some() {
            let mut analyzed = false;
            let root = parse_mysql_v2(&v, &mut analyzed);
            return Ok(Plan { root, analyzed });
        }
        Err(PlanError::NoPlan)
    }

    /// Parse MySQL's tree-format `EXPLAIN ANALYZE` (the `-> …` iterator dump — the only executed-plan
    /// format before 8.3 and the 8.x default) into the same [`Plan`] the v2-JSON parser produces (T2).
    /// The tree and the v2 JSON are the same iterator tree — v2's `operation` field literally *is* the
    /// line — so each node's kind is derived from the operation **text** the way [`mysql_v2_kind`]
    /// derives it from the `access_type` **field**. Every printed line is a node (there are no separate
    /// detail lines), and cost is a single value (no `lo..hi` range, unlike Postgres).
    pub fn from_mysql_explain_text(text: &str, _dialect: Dialect) -> Result<Plan, PlanError> {
        let lines: Vec<MyTreeLine> = text.lines().filter_map(MyTreeLine::parse).collect();
        let mut it = lines.into_iter().peekable();
        let first = it.peek().ok_or(PlanError::NoPlan)?.indent;
        let mut analyzed = false;
        let root = my_tree_subtree(&mut it, first, &mut analyzed).ok_or(PlanError::NoPlan)?;
        Ok(Plan { root, analyzed })
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

    /// Parse a DuckDB `EXPLAIN (FORMAT JSON)` document — an array of
    /// `{ "name", "children", "extra_info" }` operator nodes, already a tree.
    ///
    /// Captured from real DuckDB v1.5.5 in DD2. The vocabulary is DuckDB's own and shares nothing
    /// with the PG or MySQL documents. Note what it does *not* carry: no cost, and no actual row
    /// counts — `EXPLAIN` is estimate-only here, so `analyzed` is always false and the executed
    /// numbers come from the profiler instead (`DuckdbEnv::rows_scanned`), which is a different
    /// document entirely.
    pub fn from_duckdb_explain_json(json: &str, dialect: Dialect) -> Result<Plan, PlanError> {
        let nodes: Vec<Value> =
            serde_json::from_str(json).map_err(|e| PlanError::NotJson(e.to_string()))?;
        let mut children: Vec<PlanNode> = nodes.iter().map(|n| duckdb_node(n, dialect)).collect();
        let root = if children.len() == 1 {
            children.pop().unwrap()
        } else {
            query_root(children)
        };
        Ok(Plan {
            root,
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
        let on_table = self.nodes_on(table, alias);
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

    /// Whether an index served `column` on this occurrence, **regardless of what else that node
    /// filters**. [`Verdict`] deliberately abstains when one node both serves and rechecks a column,
    /// because for a missing-index finding that shape is ambiguous. For a sargability finding it is
    /// the answer: MySQL and MariaDB serve `col <> v` as a range over everything-except and recheck
    /// the condition on top, so served-and-filtered is exactly what a seek looks like there.
    pub fn served_by_index(&self, table: &str, alias: Option<&str>, column: &str) -> bool {
        self.nodes_on(table, alias).iter().any(|n| {
            matches!(n.access, Some(Access::IndexScan { .. }))
                && n.index_keys.iter().any(|c| c.normalized() == column)
        })
    }

    /// The row volume this plan attributes to `table` — from the **hottest** scan of it (max actual
    /// rows, falling back to estimated), with that node's estimate alongside. Lets a finding on the
    /// table be re-scored by real volume even when no column-level verdict applies. `None` when the
    /// plan never scanned the table.
    pub fn table_rows(&self, table: &str, alias: Option<&str>) -> Option<PlanRows> {
        self.nodes_on(table, alias)
            .into_iter()
            .filter(|n| n.kind == NodeKind::Scan)
            .max_by_key(|n| n.actual_rows.or(n.est_rows).unwrap_or(0))
            .map(|n| PlanRows {
                est: n.est_rows,
                actual: n.actual_rows,
            })
    }

    /// The heaviest nodes in this plan, most-costly first, each with why it is heavy — the
    /// performance-hotspots summary. Ranked by [`PlanNode::self_weight`] so a parent never
    /// outranks the child it merely contains; nodes that measure nothing (SQLite, shape-only
    /// plans) are dropped, so such a plan yields no hotspots rather than a list ordered by
    /// nothing. Pure — the analyzer fills each hotspot's `linked_rules` afterwards.
    pub fn hotspots(&self) -> Vec<Hotspot> {
        let mut ranked = Vec::new();
        collect_ranked(&self.root, false, &mut ranked);
        ranked.sort_by(|a, b| b.0.self_weight().total_cmp(&a.0.self_weight()));
        ranked
            .into_iter()
            .filter(|(n, _)| n.self_weight() > 0.0)
            .take(HOTSPOT_TOP_N)
            .map(|(n, parallel)| n.to_hotspot(parallel))
            .collect()
    }

    /// The client-facing [`DiagramNode`] tree for the web plan diagram — the plan's shape with the
    /// per-node `self_weight` (the heat key), skew factor, and spill flag precomputed, so the diagram
    /// renders numbers it never has to recompute and agrees with [`hotspots`](Self::hotspots) by
    /// construction.
    pub fn diagram(&self) -> DiagramNode {
        self.root.to_diagram()
    }

    /// [`diagram`](Self::diagram) serialized to JSON — the string the WASM `parse_plan` export hands
    /// to the web client (keeps the `serde_json` dependency in this crate, not the WASM shell).
    pub fn diagram_json(&self) -> String {
        serde_json::to_string(&self.diagram()).unwrap_or_default()
    }

    /// Nodes reading this table occurrence. A node matches when either of its names
    /// (`relation`/`alias`) equals either the base `table` or the query `alias` — engines disagree
    /// on which they report (Postgres both, MySQL the alias as relation, SQL Server the base table
    /// in `<Object>`), so the union covers all of them.
    fn nodes_on(&self, table: &str, alias: Option<&str>) -> Vec<&PlanNode> {
        let is_target =
            |nm: &Name| nm.normalized() == table || alias.is_some_and(|a| nm.normalized() == a);
        self.nodes()
            .into_iter()
            .filter(|n| {
                n.relation.as_ref().is_some_and(&is_target)
                    || n.alias.as_ref().is_some_and(&is_target)
            })
            .collect()
    }
}

/// The row volume a [`Plan`] attributes to one table's scan — its estimated and (with
/// `EXPLAIN ANALYZE`) actual row counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanRows {
    pub est: Option<u64>,
    pub actual: Option<u64>,
}

/// How many times actual rows must exceed the estimate before the planner's cardinality counts as
/// wrong. Shared by [`crate::plan`]'s skew detection and `contextualize`'s skew reframe.
pub const SKEW_FACTOR: u64 = 10;

impl PlanRows {
    /// The estimate-vs-actual skew factor (`actual / est`), `Some` only when actual dwarfs the
    /// estimate by at least [`SKEW_FACTOR`]×. `None` without both counts (an estimate-only plan) or
    /// when the estimate held — so only an `EXPLAIN ANALYZE` plan is ever skewed.
    pub fn skew_factor(&self) -> Option<u64> {
        match (self.est, self.actual) {
            (Some(est), Some(actual)) if actual >= est.max(1).saturating_mul(SKEW_FACTOR) => {
                Some(actual / est.max(1))
            }
            _ => None,
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
        loops: v.get("Actual Loops").and_then(Value::as_u64),
        est_cost: v.get("Total Cost").and_then(Value::as_f64),
        // PG reports per-loop time; the true node cost is time × loops.
        actual_time_ms: pg_actual_time(v),
        spilled: pg_spilled(v),
        rows_removed: v.get("Rows Removed by Filter").and_then(Value::as_u64),
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
    classify_kind(node_type, str_field(v, "Index Name").map(name))
}

/// The [`NodeKind`]/[`Access`] for a Postgres node type, given the index name for a scan. Shared by
/// the JSON and the plain-text parser so both map a node the same way.
fn classify_kind(node_type: &str, index: Option<Name>) -> (NodeKind, Option<Access>) {
    match node_type {
        "Seq Scan" => (NodeKind::Scan, Some(Access::SeqScan)),
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" | "Bitmap Heap Scan" => {
            (NodeKind::Scan, Some(Access::IndexScan { index }))
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

// --- Postgres plain-text parser (T1) ---

/// A parsed line of a PG text plan: a node header or a detail line, tagged with its leading-space
/// indent and whether it opens a child (the `->` marker).
struct PgTextLine {
    indent: usize,
    arrow: bool,
    content: String,
}

impl PgTextLine {
    fn parse(raw: &str) -> Option<PgTextLine> {
        let indent = raw.len() - raw.trim_start().len();
        let t = raw.trim_start();
        if t.is_empty() {
            return None;
        }
        match t.strip_prefix("->") {
            Some(rest) => Some(PgTextLine {
                indent,
                arrow: true,
                content: rest.trim_start().to_string(),
            }),
            None => Some(PgTextLine {
                indent,
                arrow: false,
                content: t.to_string(),
            }),
        }
    }
}

/// Drop the `psql` decoration (a `QUERY PLAN` header + its `---` rule, and a trailing `(N rows)`),
/// leaving the plan's own lines. Raw driver output has none of these, so each is optional.
fn strip_psql(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| {
            let t = l.trim();
            t != "QUERY PLAN"
                && !(t.starts_with('-') && t.chars().all(|c| c == '-'))
                && !(t.starts_with('(') && t.ends_with("rows)"))
                && !(t.starts_with('(') && t.ends_with("row)"))
        })
        .collect()
}

/// Build one node and its subtree: the node is the current line; following lines more indented than
/// `node_indent` are its details (non-`->`) or children (`->`), recursively. Stops at a sibling.
fn pg_text_subtree(
    it: &mut std::iter::Peekable<std::vec::IntoIter<PgTextLine>>,
    node_indent: usize,
    dialect: Dialect,
    analyzed: &mut bool,
) -> Option<PlanNode> {
    let line = it.next()?;
    let mut node = pg_text_node(&line.content, analyzed)?;
    while let Some(peek) = it.peek() {
        if peek.indent <= node_indent {
            break;
        }
        if peek.arrow {
            let child_indent = peek.indent;
            if let Some(child) = pg_text_subtree(it, child_indent, dialect, analyzed) {
                node.children.push(child);
            }
        } else {
            let detail = it.next().unwrap();
            pg_text_detail(&mut node, &detail.content, dialect);
        }
    }
    Some(node)
}

/// A node from its header line: `<type>[ on <rel>[ <alias>]][ using <idx>]  (cost=…)[ (actual …)]`.
fn pg_text_node(content: &str, analyzed: &mut bool) -> Option<PlanNode> {
    let cut = content.find("(cost=")?;
    let (raw_type, relation, alias, index) = pg_text_desc(content[..cut].trim_end());
    let (kind, access) = classify_kind(&pg_normalize_type(raw_type), index.map(str_name));
    let parens = &content[cut..];
    let actual_at = parens.find("(actual");
    let cost_part = &parens[..actual_at.unwrap_or(parens.len())];
    let (actual_rows, actual_time_ms, loops) = match actual_at.map(|i| &parens[i..]) {
        Some(a) if a.contains("never executed") => (None, None, None),
        Some(a) => {
            *analyzed = true;
            let loops = u64_after(a, "loops=");
            (
                u64_after(a, "rows="),
                cost_after(a, "time=").map(|t| t * loops.unwrap_or(1) as f64),
                loops,
            )
        }
        None => (None, None, None),
    };
    Some(PlanNode {
        kind,
        access,
        relation: relation.map(str_name),
        alias: alias.map(str_name),
        index_keys: Vec::new(),
        filtered: Vec::new(),
        est_rows: u64_after(cost_part, "rows="),
        actual_rows,
        loops,
        est_cost: cost_after(cost_part, "cost="),
        actual_time_ms,
        spilled: false,
        rows_removed: None,
        children: Vec::new(),
    })
}

/// Split a header's description into `(type, relation, alias, index)`. `Index [Only] Scan using i on t`
/// and `Seq Scan on t a` are the shapes with a relation; everything else is just a type.
fn pg_text_desc(desc: &str) -> (&str, Option<&str>, Option<&str>, Option<&str>) {
    if let Some((ty, rest)) = desc.split_once(" using ") {
        // `<idx> on <rel>[ <alias>]`
        if let Some((idx, on)) = rest.split_once(" on ") {
            let (rel, al) = pg_rel_alias(on);
            return (ty, Some(rel), al, Some(idx));
        }
        return (ty, None, None, Some(rest));
    }
    if let Some((ty, on)) = desc.split_once(" on ") {
        let (rel, al) = pg_rel_alias(on);
        return (ty, Some(rel), al, None);
    }
    (desc, None, None, None)
}

/// `orders o` → (`orders`, Some(`o`)); a schema-qualified `public.orders` → the bare `orders`, since
/// the JSON `Relation Name` is unqualified and the equality gate compares them.
fn pg_rel_alias(s: &str) -> (&str, Option<&str>) {
    let mut parts = s.split_whitespace();
    let rel = parts.next().unwrap_or(s);
    let rel = rel.rsplit('.').next().unwrap_or(rel);
    (rel, parts.next())
}

/// Strip the parallel/partial-aggregate prefixes text adds but JSON keeps as separate flags, so
/// `Parallel Seq Scan` → `Seq Scan` and `Finalize GroupAggregate` → `GroupAggregate`, mapping to the
/// same kind the JSON node does.
fn pg_normalize_type(t: &str) -> String {
    for p in ["Parallel ", "Finalize ", "Partial ", "Simple "] {
        if let Some(rest) = t.strip_prefix(p) {
            return rest.to_string();
        }
    }
    t.to_string()
}

/// Apply a detail line to its node. Only the fields the model reads: `Filter:`/`Index Cond:` columns,
/// `Rows Removed by Filter:`, and the sort/hash spill signal. Everything else is ignored.
fn pg_text_detail(node: &mut PlanNode, content: &str, dialect: Dialect) {
    if let Some(cond) = content.strip_prefix("Filter:") {
        node.filtered = cond_columns_str(cond.trim(), dialect);
    } else if let Some(cond) = content.strip_prefix("Index Cond:") {
        node.index_keys = cond_columns_str(cond.trim(), dialect);
    } else if let Some(n) = content.strip_prefix("Rows Removed by Filter:") {
        node.rows_removed = n.trim().parse().ok();
    } else if content.starts_with("Sort Method:") {
        node.spilled = content.contains("external") || content.contains("Disk:");
    }
}

/// One `-> …` line of a MySQL tree plan: its indent (the `->` column) and the operation text after it.
struct MyTreeLine {
    indent: usize,
    content: String,
}

impl MyTreeLine {
    fn parse(line: &str) -> Option<Self> {
        let arrow = line.find("->")?;
        // The tree marker is preceded only by indentation; a `->` inside an expression comes after
        // non-space text, so `find` never mistakes it for the marker.
        if !line[..arrow].bytes().all(|b| b == b' ') {
            return None;
        }
        let content = line[arrow + 2..].trim().to_string();
        (!content.is_empty()).then_some(Self {
            indent: arrow,
            content,
        })
    }
}

/// Walk a MySQL tree by relative indent: every deeper line is a child (there are no detail lines).
fn my_tree_subtree(
    it: &mut std::iter::Peekable<std::vec::IntoIter<MyTreeLine>>,
    node_indent: usize,
    analyzed: &mut bool,
) -> Option<PlanNode> {
    let line = it.next()?;
    let mut node = my_tree_node(&line.content, analyzed);
    while let Some(next) = it.peek() {
        if next.indent <= node_indent {
            break;
        }
        let indent = next.indent;
        if let Some(child) = my_tree_subtree(it, indent, analyzed) {
            node.children.push(child);
        }
    }
    Some(node)
}

/// One tree line → a [`PlanNode`], matching `parse_mysql_v2` field-for-field (rows rounded to `u64`,
/// empty `index_keys`/`filtered`, `spilled: false`, `rows_removed: None`) so text-parse == v2-parse.
fn my_tree_node(content: &str, analyzed: &mut bool) -> PlanNode {
    // A node prints its operation, then a `(cost=…)` group (absent on synthesized nodes like a count
    // or a temp-table aggregate), then — under ANALYZE — an `(actual …)` group. The description ends
    // at whichever group comes first.
    let cost_at = content.find("(cost=");
    let actual_at = content.find("(actual");
    let cut = [cost_at, actual_at]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(content.len());
    let (kind, access, relation) = my_tree_desc(content[..cut].trim_end());
    let actual = actual_at.map(|a| &content[a..]);
    let est = cost_at.map(|c| &content[c..actual_at.unwrap_or(content.len())]);
    if actual.is_some() {
        *analyzed = true;
    }
    // The tree's per-loop `time=<lo>..<hi>`, like v2's `actual_last_row_ms`, is × loops for the total.
    let loops = actual.and_then(|a| f64_after(a, "loops=")).unwrap_or(1.0);
    PlanNode {
        kind,
        access,
        relation: relation.map(name),
        alias: None,
        index_keys: Vec::new(),
        filtered: Vec::new(),
        est_rows: est.and_then(|e| f64_after(e, "rows=")).map(round_u64),
        actual_rows: actual.and_then(|a| f64_after(a, "rows=")).map(round_u64),
        loops: actual.and_then(|a| f64_after(a, "loops=")).map(round_u64),
        est_cost: est.and_then(|e| cost_after(e, "cost=")),
        actual_time_ms: actual
            .and_then(|a| cost_after(a, "time="))
            .map(|t| t * loops),
        spilled: false,
        rows_removed: None,
        children: Vec::new(),
    }
}

/// Map a tree operation phrase to `(kind, access, relation)`. The phrase carries what the v2 JSON
/// splits across `access_type` + `join_algorithm` + `index_access_type`, so it's reduced to those
/// and run through the shared [`mysql_kind`] — matching the oracle even for the pass-through
/// `Other(access_type)` kinds (`temp_table_aggregate`, `count_rows`, …).
fn my_tree_desc(desc: &str) -> (NodeKind, Option<Access>, Option<String>) {
    const SEEKS: [&str; 5] = [
        "Index lookup on ",
        "Covering index lookup on ",
        "Index range scan on ",
        "Single-row index lookup on ",
        "Single-row covering index lookup on ",
    ];
    let (access_type, hash, full) = if desc.starts_with("Table scan on ") {
        ("table", false, false)
    } else if desc.starts_with("Index scan on ") || desc.starts_with("Covering index scan on ") {
        ("index", false, true)
    } else if SEEKS.iter().any(|p| desc.starts_with(p)) {
        ("index", false, false)
    } else if desc.starts_with("Filter:") {
        ("filter", false, false)
    } else if desc.starts_with("Sort:") || desc.starts_with("Sort ") {
        ("sort", false, false)
    } else if desc.starts_with("Aggregate using temporary table") {
        ("temp_table_aggregate", false, false)
    } else if desc.starts_with("Group aggregate") {
        ("group_by", false, false)
    } else if desc.starts_with("Aggregate") {
        ("aggregate", false, false)
    } else if desc.starts_with("Limit") {
        ("limit", false, false)
    } else if desc.starts_with("Nested loop") {
        ("join", false, false)
    } else if desc.contains("hash join") {
        ("join", true, false)
    } else if desc.starts_with("Materialize") {
        ("materialized", false, false)
    } else if desc.starts_with("Count rows in ") {
        ("count_rows", false, false)
    } else {
        ("", false, false)
    };
    let (kind, access) = if access_type.is_empty() {
        (
            NodeKind::Other(desc.split_whitespace().next().unwrap_or(desc).to_string()),
            None,
        )
    } else {
        mysql_kind(access_type, hash, full)
    };
    (kind, access, my_tree_relation(desc))
}

/// A node's relation is the name after `on` (scans) or after `Count rows in` — the same value the v2
/// JSON reports as `table_name`. Other operators (join, sort, filter, aggregate) carry none.
fn my_tree_relation(desc: &str) -> Option<String> {
    desc.split(" on ")
        .nth(1)
        .or_else(|| desc.strip_prefix("Count rows in "))
        .and_then(|r| r.split_whitespace().next())
        .map(str::to_string)
}

/// The first numeric token after `key`, e.g. `rows=0.98` → `0.98` (MySQL reports fractional
/// per-loop rows; the caller rounds to match the v2 JSON's `round_u64`).
fn f64_after(s: &str, key: &str) -> Option<f64> {
    let i = s.find(key)? + key.len();
    s[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .parse()
        .ok()
}

/// The first `digits` token after `key`, e.g. `rows=` → `9833`.
fn u64_after(s: &str, key: &str) -> Option<u64> {
    let i = s.find(key)? + key.len();
    s[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// The upper bound of a `key=<lo>..<hi>` range (cost/time), e.g. `cost=0.00..3582.00` → `3582.00`.
fn cost_after(s: &str, key: &str) -> Option<f64> {
    let i = s.find(key)? + key.len();
    let tok: String = s[i..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    tok.rsplit_once("..")
        .map_or_else(|| tok.parse().ok(), |(_, hi)| hi.parse().ok())
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
        loops: None,
        est_cost: None,
        actual_time_ms: None,
        spilled: false,
        rows_removed: None,
        children,
    }
}

/// Walk a MySQL `query_block` tree, emitting one leaf node per table access (including tables
/// reached through nested subqueries). Mirrors the verify-framework walk.
fn collect_mysql_tables(v: &Value, dialect: Dialect, out: &mut Vec<PlanNode>, analyzed: &mut bool) {
    if let Some(map) = v.as_object() {
        if str_field(v, "table_name").is_some() && str_field(v, "access_type").is_some() {
            out.push(mysql_table_node(v, dialect, analyzed));
        }
        for child in map.values() {
            collect_mysql_tables(child, dialect, out, analyzed);
        }
    } else if let Some(arr) = v.as_array() {
        arr.iter()
            .for_each(|e| collect_mysql_tables(e, dialect, out, analyzed));
    }
}

/// One MySQL table access → a scan node. `ALL`/`index` are full scans ("no seek") → `SeqScan` with
/// no served columns; a keyed access (`ref`/`eq_ref`/`range`/`const`) is an index seek whose
/// `used_key_parts` are the served columns. `attached_condition` gives the post-scan filter.
fn mysql_table_node(v: &Value, dialect: Dialect, analyzed: &mut bool) -> PlanNode {
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
        // Estimate: MySQL nests it under `cost_info`; MariaDB puts flat `rows`/`cost` on the table.
        est_rows: v
            .get("cost_info")
            .and_then(|c| c.get("prefix_rows"))
            .and_then(Value::as_u64)
            .or_else(|| v.get("rows").and_then(Value::as_u64)),
        // Actual (MariaDB `ANALYZE FORMAT=JSON`): `r_rows` is a per-loop average, so the node total
        // is `r_rows × r_loops`. Absent on estimated `EXPLAIN` and on MySQL's v1 (which is
        // estimate-only — its executed form is the v2 iterator tree).
        actual_rows: mariadb_actual_rows(v, analyzed),
        // MariaDB already folds loops into `r_rows × r_loops`; MySQL v1 is estimate-only.
        loops: None,
        est_cost: v
            .get("cost_info")
            .and_then(|c| c.get("read_cost"))
            .and_then(json_f64)
            .or_else(|| v.get("cost").and_then(json_f64)),
        // MariaDB reports the node's time split as `r_table_time_ms` (in the engine) + `r_other_time_ms`;
        // there is no `r_total_time_ms` on the table node. Their sum is the node total.
        actual_time_ms: mariadb_node_time(v),
        spilled: false,
        rows_removed: None,
        children: Vec::new(),
    }
}

/// MariaDB `ANALYZE` actual rows for a table node: `r_rows` (per-loop average) × `r_loops`. Sets
/// `analyzed` when the runtime field is present. `None` on an estimated `EXPLAIN`.
fn mariadb_actual_rows(v: &Value, analyzed: &mut bool) -> Option<u64> {
    let r_rows = v.get("r_rows").and_then(json_f64)?;
    *analyzed = true;
    let loops = v.get("r_loops").and_then(json_f64).unwrap_or(1.0);
    Some((r_rows * loops).round() as u64)
}

/// MariaDB `ANALYZE` node time: `r_table_time_ms` + `r_other_time_ms` (either may be absent). `None`
/// when neither is present (an estimated `EXPLAIN`).
fn mariadb_node_time(v: &Value) -> Option<f64> {
    let table = v.get("r_table_time_ms").and_then(json_f64);
    let other = v.get("r_other_time_ms").and_then(json_f64);
    match (table, other) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
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
        loops: v.get("actual_loops").and_then(Value::as_f64).map(round_u64),
        est_cost: v.get("estimated_total_cost").and_then(json_f64),
        // v2 reports per-loop `actual_last_row_ms`; the node's total time is that × loops.
        actual_time_ms: mysql_v2_time(v),
        spilled: false,
        rows_removed: None,
        children,
    }
}

/// A MySQL v2 node's `access_type` (refined by `index_access_type` / `join_algorithm`) → its kind
/// and, for a table access, its [`Access`].
fn mysql_v2_kind(v: &Value, access_type: &str) -> (NodeKind, Option<Access>) {
    let hash_join = str_field(v, "join_algorithm").as_deref() == Some("hash");
    // A full index scan serves no seek; a lookup/ref/range does.
    let full_index = str_field(v, "index_access_type").as_deref() == Some("index_scan");
    mysql_kind(access_type, hash_join, full_index)
}

/// Map a MySQL `access_type` (plus the two discriminators the tree carries in its phrase) to a
/// [`NodeKind`]/[`Access`]. Shared by the v2-JSON and text-tree parsers so an unmapped `access_type`
/// yields the *same* `Other(access_type)` on both — the T2 equality gate depends on it.
fn mysql_kind(access_type: &str, hash_join: bool, full_index: bool) -> (NodeKind, Option<Access>) {
    match access_type {
        "table" => (NodeKind::Scan, Some(Access::SeqScan)),
        "index" if full_index => (NodeKind::Scan, Some(Access::SeqScan)),
        "index" => (NodeKind::Scan, Some(Access::IndexScan { index: None })),
        "join" if hash_join => (NodeKind::HashJoin, None),
        "join" => (NodeKind::NestedLoop, None),
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
        loops: None,
        est_cost: None,
        actual_time_ms: None,
        spilled: false,
        rows_removed: None,
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

/// One DuckDB operator node → a plan node, recursing over `children`.
///
/// `extra_info` carries `Table` (catalog-qualified, e.g. `memory.main.orders`), `Filters`, and
/// `Estimated Cardinality`. `Filters` is a real predicate string, so it goes through the same
/// `cond_columns_str` parse the other backends use — which is what lets the `unindexed-*` rules
/// see which columns were filtered after the scan rather than served by an index.
/// DuckDB's file-reader operators. `read_parquet(...)` and a bare `FROM 'x.parquet'` produce
/// different names for the same thing, and neither carries a `Table` key — they carry `Function` —
/// so `relation` stays `None` for them. That is deliberate: the only file identity available is the
/// path, and paths do not go into findings (see `docs/phase-dd5b-path-literal-shape.md`).
macro_rules! FILE_SCANS {
    () => {
        "READ_PARQUET"
            | "PARQUET_SCAN"
            | "READ_CSV"
            | "READ_CSV_AUTO"
            | "READ_JSON"
            | "READ_JSON_AUTO"
            | "ARROW_SCAN"
    };
}

fn duckdb_node(v: &Value, dialect: Dialect) -> PlanNode {
    let name = v.get("name").and_then(Value::as_str).unwrap_or_default();
    let info = |k: &str| {
        v.get("extra_info")
            .and_then(|e| e.get(k))
            .and_then(Value::as_str)
    };
    let access = match name {
        // DuckDB's only index is the ART, which serves point lookups — it is not the ordered
        // range structure the row stores mean by "index scan". DD3a is what settles whether any
        // rule should read this as one.
        "INDEX_SCAN" => Some(Access::IndexScan {
            index: info("Index").map(|i| Name::new(i, false)),
        }),
        "SEQ_SCAN" | "TABLE_SCAN" => Some(Access::SeqScan),
        // A file source is a scan, and until this arm existed DuckDB — the dialect whose entire
        // premise is file sources — produced plans with no `Scan` node in them at all, so
        // `Plan::table_rows`, hotspot attribution and every scan-keyed rule saw nothing. There is
        // no index over a Parquet or CSV file, so the access is sequential by construction.
        FILE_SCANS!() => Some(Access::SeqScan),
        _ => None,
    };
    let is_scan = access.is_some();
    PlanNode {
        kind: match name {
            "SEQ_SCAN" | "TABLE_SCAN" | "INDEX_SCAN" | FILE_SCANS!() => NodeKind::Scan,
            // A `WITH` body and the reads of it. Every other backend maps its equivalent
            // (`Materialize`/`CTE Scan`, `materialized`, `Table Spool`); DuckDB did not, so the
            // one barrier a plan can see was invisible on it.
            "CTE" | "CTE_SCAN" => NodeKind::Materialize,
            // DuckDB emits the duplicate-eliminated join as `LEFT_DELIM_JOIN`/`RIGHT_DELIM_JOIN`;
            // bare `DELIM_JOIN` was what this arm originally guessed and is not a name the engine
            // uses, so every delim join was classified `Other` and no join rule could see one. The
            // real names came out of surveying TPC-H plans (PS1).
            "HASH_JOIN" | "DELIM_JOIN" | "LEFT_DELIM_JOIN" | "RIGHT_DELIM_JOIN" => {
                NodeKind::HashJoin
            }
            "NESTED_LOOP_JOIN" | "BLOCKWISE_NL_JOIN" | "CROSS_PRODUCT" => NodeKind::NestedLoop,
            "PIECEWISE_MERGE_JOIN" | "IEJOIN" => NodeKind::MergeJoin,
            "ORDER_BY" | "TOP_N" => NodeKind::Sort,
            "HASH_GROUP_BY" | "PERFECT_HASH_GROUP_BY" | "UNGROUPED_AGGREGATE" => {
                NodeKind::Aggregate
            }
            "LIMIT" | "STREAMING_LIMIT" => NodeKind::Limit,
            other => NodeKind::Other(other.to_string()),
        },
        access,
        // A scan names its table; a `CTE` node names the CTE. Both are the thing a finding would
        // point at, and without the second a `WITH` body was an anonymous barrier in every hotspot.
        // `CTE_SCAN` carries only a `CTE Index`, and resolving that to a name needs a second pass
        // with no caller today, so a reference stays unnamed.
        relation: is_scan
            .then(|| info("Table"))
            .flatten()
            .map(|t| Name::new(t.rsplit('.').next().unwrap_or(t), false))
            .or_else(|| info("CTE Name").map(|c| Name::new(c, false))),
        alias: None,
        index_keys: Vec::new(),
        // A scan reports pushed-down predicates as `Filters`; a standalone `FILTER` operator reports
        // the predicate it applies as `Expression`. Only the first was read, so a filter that did
        // **not** push into the scan — the interesting case, since it is the one that costs
        // something — reached the model as nothing at all.
        filtered: info("Filters")
            .or_else(|| (name == "FILTER").then(|| info("Expression")).flatten())
            .map(|f| cond_columns_str(f, dialect))
            .unwrap_or_default(),
        est_rows: info("Estimated Cardinality").and_then(|c| c.trim().parse().ok()),
        actual_rows: None,
        loops: None,
        est_cost: None,
        actual_time_ms: None,
        spilled: false,
        rows_removed: None,
        children: v
            .get("children")
            .and_then(Value::as_array)
            .map(|c| c.iter().map(|n| duckdb_node(n, dialect)).collect())
            .unwrap_or_default(),
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
        // SQL Server's `ActualRows` is already summed across executions, so no separate loop scaling.
        loops: None,
        est_cost: attr_f64(reloop, "EstimatedTotalSubtreeCost"),
        actual_time_ms,
        spilled: mssql_spilled(reloop),
        rows_removed: None,
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

    /// Captured verbatim from real DuckDB v1.5.5 (`EXPLAIN (FORMAT JSON) SELECT count(*) FROM t
    /// WHERE k = 3`), DD2. A synthetic fixture would only prove the parser matches my guess at
    /// the format.
    const DUCKDB_EXPLAIN: &str = r#"[
        { "name": "UNGROUPED_AGGREGATE",
          "children": [
            { "name": "SEQ_SCAN", "children": [],
              "extra_info": { "Table": "memory.main.t", "Type": "Sequential Scan",
                              "Projections": "", "Filters": "k=3",
                              "Estimated Cardinality": "42858" } } ],
          "extra_info": { "Aggregates": "count_star()" } } ]"#;

    #[test]
    fn duckdb_explain_json_parses() {
        let plan = Plan::from_duckdb_explain_json(DUCKDB_EXPLAIN, Dialect::Duckdb).unwrap();
        assert!(!plan.analyzed, "DuckDB EXPLAIN is estimate-only");
        let nodes = plan.nodes();
        assert_eq!(nodes[0].kind, NodeKind::Aggregate);

        let scan = nodes
            .iter()
            .find(|n| n.kind == NodeKind::Scan)
            .expect("a scan node");
        assert!(matches!(scan.access, Some(Access::SeqScan)));
        // Catalog-qualified in the document; the leaf name is what a rule matches on.
        assert_eq!(scan.relation.as_ref().unwrap().normalized(), "t");
        assert_eq!(scan.est_rows, Some(42858));
        // `Filters` is a real predicate, so the filtered column is recoverable — that is what the
        // `unindexed-*` family reads.
        assert_eq!(
            scan.filtered
                .iter()
                .map(|c| c.normalized())
                .collect::<Vec<_>>(),
            ["k"]
        );
    }

    #[test]
    fn duckdb_explain_routes_through_from_explain() {
        let plan = Plan::from_explain(DUCKDB_EXPLAIN, Dialect::Duckdb).unwrap();
        assert!(plan.nodes().iter().any(|n| n.kind == NodeKind::Scan));
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

    #[test]
    fn table_rows_picks_the_hottest_scan_actuals() {
        // Two scans of `orders`; table_rows returns the one with the most actual rows.
        let p = pg(r#"[{"Plan":{"Node Type":"Nested Loop","Plans":[
            {"Node Type":"Index Scan","Relation Name":"orders","Plan Rows":10,"Actual Rows":8},
            {"Node Type":"Seq Scan","Relation Name":"orders","Plan Rows":900,"Actual Rows":4200000}]}}]"#);
        assert_eq!(
            p.table_rows("orders", None),
            Some(PlanRows {
                est: Some(900),
                actual: Some(4200000)
            })
        );
        assert_eq!(p.table_rows("missing", None), None);
    }

    #[test]
    fn table_rows_falls_back_to_estimate_without_analyze() {
        let p =
            pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"users","Plan Rows":123}}]"#);
        assert_eq!(
            p.table_rows("users", None),
            Some(PlanRows {
                est: Some(123),
                actual: None
            })
        );
    }

    // An analyzed PG plan: a Sort over a Hash Join over two Seq Scans, with realistic per-node
    // Actual Total Time (inclusive of children, as PG reports it).
    const ANALYZED_PLAN: &str = r#"[{"Plan":{"Node Type":"Sort","Actual Total Time":50.0,
        "Actual Loops":1,"Plan Rows":900,"Actual Rows":900,"Sort Method":"external merge","Plans":[
        {"Node Type":"Hash Join","Actual Total Time":45.0,"Actual Loops":1,"Plans":[
          {"Node Type":"Seq Scan","Relation Name":"orders","Actual Total Time":40.0,
           "Actual Loops":1,"Plan Rows":1000000,"Actual Rows":1000000},
          {"Node Type":"Seq Scan","Relation Name":"customers","Actual Total Time":1.0,
           "Actual Loops":1,"Plan Rows":100,"Actual Rows":100}]}]}}]"#;

    #[test]
    fn hotspots_rank_by_self_weight_not_inclusive_time() {
        // The Sort root has the largest inclusive time (50ms) but almost no self-time; the orders
        // Seq Scan (40ms, no children) is where the cost actually is, so it ranks first.
        let hs = pg(ANALYZED_PLAN).hotspots();
        assert_eq!(hs[0].kind, NodeKind::Scan);
        assert_eq!(hs[0].relation.as_deref(), Some("orders"));
        // The tiny customers scan (1ms) is below the heavier interior nodes.
        let orders_at = hs
            .iter()
            .position(|h| h.cause == HotspotCause::SeqScanReturningRows);
        assert_eq!(orders_at, Some(0));
    }

    #[test]
    fn hotspots_cause_reads_the_node_shape() {
        let hs = pg(ANALYZED_PLAN).hotspots();
        // The disk-sort root surfaces its spill as the cause, ahead of the access-shape default.
        let sort = hs.iter().find(|h| h.kind == NodeKind::Sort).unwrap();
        assert_eq!(sort.cause, HotspotCause::SortSpilled);
        assert!(sort.spilled);
    }

    #[test]
    fn hotspots_cause_flags_estimate_skew() {
        // Actual rows 100× the estimate — a blown cardinality outranks the seq-scan shape.
        let p = pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t",
            "Actual Total Time":9.0,"Actual Loops":1,"Plan Rows":100,"Actual Rows":10000}}]"#);
        assert_eq!(
            p.hotspots()[0].cause,
            HotspotCause::EstimateActualSkew { factor: 100 }
        );
    }

    #[test]
    fn hotspots_drop_zero_weight_nodes() {
        // SQLite / shape-only plans measure nothing (no time, cost, or rows) → no hotspots.
        let p = pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t"}}]"#);
        assert!(p.hotspots().is_empty());
    }

    #[test]
    fn hotspots_cap_at_top_n() {
        // Six scans under a join, each with its own cost → only the top five are reported.
        let scans = (0..6)
            .map(|i| {
                format!(
                    r#"{{"Node Type":"Seq Scan","Relation Name":"t{i}","Total Cost":{},"Plan Rows":10}}"#,
                    (i + 1) * 100
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let p = pg(&format!(
            r#"[{{"Plan":{{"Node Type":"Hash Join","Total Cost":10000,"Plans":[{scans}]}}}}]"#
        ));
        assert_eq!(p.hotspots().len(), HOTSPOT_TOP_N);
    }

    #[test]
    fn hotspots_flag_worker_summed_time_only_under_a_gather() {
        // A Parallel Seq Scan under a Gather: its 456ms is 152ms × 3 workers, exceeding the 180ms
        // the Gather (leader wall clock) reports — so the scan hotspot is flagged worker-summed.
        let par = pg(
            r#"[{"Plan":{"Node Type":"Gather","Actual Total Time":180.0,"Actual Loops":1,
            "Plans":[{"Node Type":"Seq Scan","Relation Name":"orders","Actual Total Time":152.0,
            "Actual Loops":3,"Plan Rows":1000000,"Actual Rows":1000000}]}}]"#,
        );
        let scan = par
            .hotspots()
            .into_iter()
            .find(|h| h.kind == NodeKind::Scan)
            .unwrap();
        assert!(
            scan.worker_summed_time,
            "a scan under a Gather sums worker time"
        );

        // No Gather in ANALYZED_PLAN → the same shape of scan is plain wall-clock.
        let serial = pg(ANALYZED_PLAN)
            .hotspots()
            .into_iter()
            .find(|h| h.relation.as_deref() == Some("orders"))
            .unwrap();
        assert!(!serial.worker_summed_time);
    }

    #[test]
    fn diagram_projection_carries_label_weight_and_skew() {
        // A Seq Scan under a Hash Join; the scan under-estimated 100 → 5000 rows.
        let p = pg(
            r#"[{"Plan":{"Node Type":"Hash Join","Total Cost":1000,"Plan Rows":5000,"Plans":[
            {"Node Type":"Seq Scan","Relation Name":"orders","Total Cost":800,"Plan Rows":100,
             "Actual Rows":5000}]}}]"#,
        );
        let d = p.diagram();
        assert_eq!(d.label, "Hash Join");
        assert_eq!(d.children.len(), 1);
        let scan = &d.children[0];
        assert_eq!(scan.label, "Seq Scan");
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.skew_factor, Some(50)); // 5000 / 100
                                                // Additive by cost; the leaf scan has no children, so its own cost is its self-weight.
        assert_eq!(scan.self_weight, 800.0);
        // The parent's self-weight excludes the child it contains: 1000 - 800.
        assert_eq!(d.self_weight, 200.0);
        assert!(p.diagram_json().contains("Hash Join"));
        // A blown estimate outranks the access shape, so the scan reads as a skew concern.
        assert_eq!(scan.health, NodeHealth::Skew);
    }

    #[test]
    fn diagram_health_reflects_access_shape_not_share() {
        use NodeHealth::*;
        // Seq scan that discards almost everything it reads → a missing/unselective index (bad).
        let bad_scan = pg(
            r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Plan Rows":200000,
              "Actual Rows":10,"Rows Removed by Filter":200000}}]"#,
        )
        .diagram();
        assert_eq!(bad_scan.health, InefficientScan);

        // A full sequential scan of a big table → an index would help → bad (red).
        let big_scan = pg(
            r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Plan Rows":200000,
              "Actual Rows":200000}}]"#,
        )
        .diagram();
        assert_eq!(big_scan.health, InefficientScan);

        // A mid-size seq scan (10k–50k) is worth attention but not clearly bad → yellow.
        let mid_scan =
            pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Actual Rows":20000}}]"#)
                .diagram();
        assert_eq!(mid_scan.health, LargeScan);

        // A tight index seek returning what it reads → the ideal access (good), whatever its cost.
        let seek = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"t","Index Name":"t_pk",
              "Total Cost":99999,"Plan Rows":1,"Actual Rows":1}}]"#,
        )
        .diagram();
        assert_eq!(seek.health, Efficient);

        // The same seek re-run 200k times under a nested loop: "1 row" per loop hides 200k fetches →
        // a concern, not the green it looks at face value. The loop count is surfaced for the badge.
        let looped = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"d","Index Name":"d_pk",
              "Plan Rows":1,"Actual Rows":1,"Actual Loops":200000}}]"#,
        )
        .diagram();
        assert_eq!(looped.health, LargeScan);
        assert_eq!(looped.loops, Some(200000));

        // A large index scan (100k rows = 100k clustered fetches) is not "efficient" either.
        let wide_index = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"t","Index Name":"t_k",
              "Plan Rows":100000,"Actual Rows":100000}}]"#,
        )
        .diagram();
        assert_eq!(wide_index.health, LargeScan);

        // A small scan and a plain operator are fine → green (measured, nothing to flag).
        let small =
            pg(r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Actual Rows":50}}]"#)
                .diagram();
        assert_eq!(small.health, Efficient);
        let join = pg(
            r#"[{"Plan":{"Node Type":"Hash Join","Total Cost":500,"Plans":[
              {"Node Type":"Seq Scan","Relation Name":"t","Actual Rows":10}]}}]"#,
        )
        .diagram();
        assert_eq!(join.health, Efficient);

        // A shape-only node with no measured rows/time/cost stays neutral.
        let shapeless = pg(r#"[{"Plan":{"Node Type":"Result"}}]"#).diagram();
        assert_eq!(shapeless.health, Neutral);
    }

    #[test]
    fn diagram_labels_an_index_scan_with_its_index() {
        let p = pg(
            r#"[{"Plan":{"Node Type":"Index Scan","Relation Name":"users",
            "Index Name":"users_email_idx","Index Cond":"(email = 'x')"}}]"#,
        );
        let d = p.diagram();
        assert_eq!(d.label, "Index Scan");
        assert_eq!(d.index.as_deref(), Some("users_email_idx"));
    }

    // Real PG text captured from a live container (`ztmp_pg_text_shapes`), trimmed.
    #[test]
    fn pg_text_seq_scan_with_filter() {
        let p = Plan::from_explain(
            "Seq Scan on orders  (cost=0.00..3582.00 rows=9833 width=4) (actual time=0.008..16.249 rows=10000 loops=1)\n  Filter: (status = 'x'::text)\n  Rows Removed by Filter: 190000\nPlanning Time: 0.109 ms\nExecution Time: 16.669 ms",
            Dialect::Postgres,
        )
        .unwrap();
        assert!(p.analyzed);
        let n = &p.root;
        assert_eq!(n.kind, NodeKind::Scan);
        assert!(matches!(n.access, Some(Access::SeqScan)));
        assert_eq!(n.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(n.est_rows, Some(9833));
        assert_eq!(n.actual_rows, Some(10000));
        assert_eq!(n.rows_removed, Some(190000));
        assert_eq!(n.est_cost, Some(3582.0));
        assert_eq!(n.actual_time_ms, Some(16.249)); // × 1 loop
        assert_eq!(cols(&n.filtered), ["status"]);
    }

    #[test]
    fn pg_text_tolerates_psql_decoration() {
        let raw = "                    QUERY PLAN\n------------------------------------\n Seq Scan on orders  (cost=0.00..3582.00 rows=9833 width=4)\n   Filter: (status = 'x'::text)\n   Rows Removed by Filter: 190000\n(3 rows)";
        let p = Plan::from_explain(raw, Dialect::Postgres).unwrap();
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(p.root.rows_removed, Some(190000));
        assert!(!p.analyzed, "estimate-only, no actual group");
    }

    #[test]
    fn pg_text_builds_the_join_tree() {
        let t = "Hash Join  (cost=28.50..3636.42 rows=9833 width=8) (actual time=0.255..18.511 rows=9800 loops=1)\n  Hash Cond: (o.cid = c.id)\n  ->  Seq Scan on orders o  (cost=0.00..3582.00 rows=9833 width=8) (actual time=0.008..16.502 rows=10000 loops=1)\n        Filter: (status = 'x'::text)\n        Rows Removed by Filter: 190000\n  ->  Hash  (cost=16.00..16.00 rows=1000 width=8) (actual time=0.241..0.243 rows=1000 loops=1)\n        ->  Seq Scan on customers c  (cost=0.00..16.00 rows=1000 width=8) (actual time=0.003..0.086 rows=1000 loops=1)";
        let p = Plan::from_explain(t, Dialect::Postgres).unwrap();
        assert_eq!(p.root.kind, NodeKind::HashJoin);
        assert_eq!(p.root.children.len(), 2);
        assert_eq!(
            p.root.children[0].relation.as_ref().unwrap().normalized(),
            "orders"
        );
        assert_eq!(p.root.children[0].alias.as_ref().unwrap().normalized(), "o");
        assert_eq!(p.root.children[1].kind, NodeKind::Hash);
        assert_eq!(
            p.root.children[1].children[0]
                .relation
                .as_ref()
                .unwrap()
                .normalized(),
            "customers"
        );
    }

    #[test]
    fn pg_text_index_only_scan() {
        let t = "Index Only Scan using o_id on orders  (cost=0.42..8.44 rows=1 width=4) (actual time=0.027..0.028 rows=1 loops=1)\n  Index Cond: (id = 5)";
        let p = Plan::from_explain(t, Dialect::Postgres).unwrap();
        assert!(
            matches!(&p.root.access, Some(Access::IndexScan { index }) if index.as_ref().unwrap().normalized() == "o_id")
        );
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(cols(&p.root.index_keys), ["id"]);
    }

    #[test]
    fn pg_text_never_executed_parallel_and_spill() {
        // `Parallel Seq Scan` normalizes to a plain scan; `(never executed)` → no actuals; the
        // external sort spills.
        let t = "Gather  (cost=1000.00..2000.00 rows=100 width=4) (actual time=1.0..2.0 rows=50 loops=1)\n  ->  Parallel Seq Scan on orders  (cost=0.00..1000.00 rows=50 width=4) (never executed)\n  ->  Sort  (cost=5.00..6.00 rows=1 width=4) (actual time=3.0..4.0 rows=1 loops=1)\n        Sort Method: external merge  Disk: 100kB";
        let p = Plan::from_explain(t, Dialect::Postgres).unwrap();
        assert_eq!(p.root.children.len(), 2);
        let scan = &p.root.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert!(matches!(scan.access, Some(Access::SeqScan)));
        assert_eq!(scan.actual_rows, None, "never executed carries no actuals");
        assert!(p.root.children[1].spilled);
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

    /// Real MariaDB 11.4 documents (captured in `crates/verify/tests/mariadb.rs`) through the
    /// production entry point. MA1 routed `Dialect::Mariadb` at the MySQL parser provisionally;
    /// this is the evidence that the routing is right — MariaDB uses the same `query_block` /
    /// `table` / `access_type` vocabulary, so a user-supplied MariaDB plan is understood rather
    /// than silently mis-read.
    #[test]
    fn mariadb_document_drives_the_same_verdicts() {
        let scan = Plan::from_explain(
            r#"{"query_block":{"select_id":1,"cost":0.3368548,"nested_loop":[{"table":{
                "table_name":"t","access_type":"ALL","loops":1,"rows":2000,"cost":0.3368548,
                "filtered":100,"attached_condition":"t.`name` = 'nope'"}}]}}"#,
            Dialect::Mariadb,
        )
        .unwrap();
        assert_eq!(
            scan.verdict("t", None, "name"),
            Verdict::Confirm { actual_rows: None }
        );

        let seek = Plan::from_explain(
            r#"{"query_block":{"select_id":1,"cost":0.001792605,"nested_loop":[{"table":{
                "table_name":"t","access_type":"ref","possible_keys":["t_k"],"key":"t_k",
                "used_key_parts":["k"],"loops":1,"rows":1,"cost":0.001792605,"filtered":100,
                "using_index":true}}]}}"#,
            Dialect::Mariadb,
        )
        .unwrap();
        assert_eq!(seek.verdict("t", None, "k"), Verdict::Suppress);
    }

    #[test]
    fn mariadb_analyze_reads_actuals_and_marks_analyzed() {
        // Real MariaDB `ANALYZE FORMAT=JSON` shape: flat `rows`/`cost` on the table, plus
        // `r_rows` (per-loop) / `r_loops` / `r_table_time_ms` + `r_other_time_ms`.
        let p = Plan::from_explain(
            r#"{"query_block":{"nested_loop":[{"table":{"table_name":"t","access_type":"ALL",
              "rows":1000,"cost":1.5,"r_loops":2,"r_rows":25000,"r_table_time_ms":40.0,
              "r_other_time_ms":2.0}}]}}"#,
            Dialect::Mariadb,
        )
        .unwrap();
        assert!(p.analyzed, "r_rows present ⇒ analyzed");
        let n = p.table_rows("t", None).unwrap();
        assert_eq!(n.est, Some(1000)); // flat `rows`
        assert_eq!(n.actual, Some(50000)); // r_rows 25000 × r_loops 2
        assert_eq!(n.skew_factor(), Some(50)); // 50000 / 1000 — skew now computable on MariaDB
        let node = p
            .nodes()
            .into_iter()
            .find(|x| x.relation.as_ref().map(Name::normalized).as_deref() == Some("t"))
            .unwrap();
        assert_eq!(node.actual_time_ms, Some(42.0)); // r_table_time_ms + r_other_time_ms
    }

    #[test]
    fn mariadb_estimated_explain_stays_unanalyzed() {
        // No `r_*` ⇒ an estimated EXPLAIN: estimates read, actuals absent, not analyzed.
        let p = Plan::from_explain(
            r#"{"query_block":{"nested_loop":[{"table":{"table_name":"t","access_type":"ALL",
              "rows":1000,"cost":1.5}}]}}"#,
            Dialect::Mariadb,
        )
        .unwrap();
        assert!(!p.analyzed);
        let n = p.table_rows("t", None).unwrap();
        assert_eq!(n.est, Some(1000));
        assert_eq!(n.actual, None);
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

    fn mysql_tree(text: &str) -> Plan {
        Plan::from_explain(text, Dialect::Mysql).unwrap()
    }

    #[test]
    fn mysql_tree_filter_over_table_scan() {
        // Real MySQL 8.4 `EXPLAIN ANALYZE` (tree). A filter's access_type is `filter` → `Other`.
        let p = mysql_tree(
            "-> Filter: (orders.`status` = 'x')  (cost=20104 rows=19968) (actual time=5.75..1183 rows=10000 loops=1)\n    -> Table scan on orders  (cost=20104 rows=199680) (actual time=0.0708..982 rows=200000 loops=1)",
        );
        assert!(p.analyzed);
        assert_eq!(p.root.kind, NodeKind::Other("filter".into()));
        assert!(p.root.access.is_none());
        assert!(p.root.relation.is_none());
        assert_eq!(p.root.est_rows, Some(19968));
        assert_eq!(p.root.actual_rows, Some(10000));
        assert_eq!(p.root.est_cost, Some(20104.0));
        assert_eq!(p.root.actual_time_ms, Some(1183.0));
        let scan = &p.root.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert!(matches!(scan.access, Some(Access::SeqScan)));
        assert_eq!(scan.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(scan.est_rows, Some(199680));
        assert_eq!(scan.actual_rows, Some(200000));
    }

    #[test]
    fn mysql_tree_covering_index_lookup_is_a_seek() {
        let p = mysql_tree(
            "-> Covering index lookup on orders using o_id (id=5)  (cost=0.35 rows=1) (actual time=0.022..0.0261 rows=1 loops=1)",
        );
        assert_eq!(p.root.kind, NodeKind::Scan);
        assert!(matches!(p.root.access, Some(Access::IndexScan { .. })));
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(p.root.est_cost, Some(0.35));
    }

    #[test]
    fn mysql_tree_nested_loop_rounds_fractional_rows() {
        // The inner side runs 10000 loops and reports a fractional per-loop `rows=0.98` — it must
        // round to 1, exactly as the v2 JSON's `round_u64(0.98)` does, or the equality gate breaks.
        let p = mysql_tree(
            "-> Nested loop inner join  (cost=27093 rows=19968) (actual time=0.0555..139 rows=9800 loops=1)\n    -> Filter: ((orders.`status` = 'x'))  (cost=20104 rows=19968) (actual time=0.0422..124 rows=10000 loops=1)\n        -> Table scan on orders  (cost=20104 rows=199680) (actual time=0.0257..104 rows=200000 loops=1)\n    -> Single-row index lookup on customers using PRIMARY (id=orders.cid)  (cost=0.25 rows=1) (actual time=0.00129..0.00133 rows=0.98 loops=10000)",
        );
        assert_eq!(p.root.kind, NodeKind::NestedLoop);
        assert_eq!(p.root.children.len(), 2);
        let filter = &p.root.children[0];
        assert_eq!(filter.kind, NodeKind::Other("filter".into()));
        assert_eq!(
            filter.children[0].relation.as_ref().unwrap().normalized(),
            "orders"
        );
        let inner = &p.root.children[1];
        assert!(matches!(inner.access, Some(Access::IndexScan { .. })));
        assert_eq!(inner.relation.as_ref().unwrap().normalized(), "customers");
        assert_eq!(inner.actual_rows, Some(1));
        // per-loop 0.00133ms × 10000 loops.
        assert_eq!(inner.actual_time_ms, Some(0.00133 * 10000.0));
    }

    #[test]
    fn mysql_tree_temp_aggregate_and_costless_nodes() {
        // A GROUP BY via temp table: nodes with no `(cost=…)` group carry no est rows/cost, and the
        // aggregate's `temp_table_aggregate` access_type passes through to `Other` on both paths.
        let p = mysql_tree(
            "-> Table scan on <temporary>  (actual time=1867..1867 rows=2 loops=1)\n    -> Aggregate using temporary table  (actual time=1867..1867 rows=2 loops=1)\n        -> Table scan on orders  (cost=20104 rows=199680) (actual time=0.0473..886 rows=200000 loops=1)",
        );
        assert_eq!(p.root.kind, NodeKind::Scan);
        assert_eq!(
            p.root.relation.as_ref().unwrap().normalized(),
            "<temporary>"
        );
        assert_eq!(p.root.est_rows, None);
        assert_eq!(p.root.est_cost, None);
        assert_eq!(p.root.actual_rows, Some(2));
        let agg = &p.root.children[0];
        assert_eq!(agg.kind, NodeKind::Other("temp_table_aggregate".into()));
        assert_eq!(agg.est_rows, None);
        assert_eq!(
            agg.children[0].relation.as_ref().unwrap().normalized(),
            "orders"
        );
    }

    #[test]
    fn mysql_tree_count_rows_carries_relation() {
        let p = mysql_tree("-> Count rows in orders  (actual time=608..608 rows=1 loops=1)");
        assert_eq!(p.root.kind, NodeKind::Other("count_rows".into()));
        assert_eq!(p.root.relation.as_ref().unwrap().normalized(), "orders");
        assert_eq!(p.root.actual_rows, Some(1));
        assert_eq!(p.root.est_rows, None);
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
