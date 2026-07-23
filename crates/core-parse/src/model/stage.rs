//! Stages and relations — the tree structure of a query.
//!
//! A [`Relation`] produces a table: either a single SELECT-style [`Stage`], or a
//! [`SetOp`] combining two relations. The FROM clause is a join *tree* (D3) so join
//! type and structure survive; set operations are their own node (D4) so every
//! `Stage` unambiguously means one SELECT.

use super::expr::{Expr, SourceId};
use super::name::{Name, Span, TableName};
use super::query::Cte;
use super::ty::Type;

/// Anything that produces a table.
#[derive(Debug, Clone)]
pub enum Relation {
    Stage(Box<Stage>),
    SetOp(Box<SetOp>),
}

/// A single SELECT-style operation. Every field always has a well-defined meaning
/// (no field's interpretation depends on another).
#[derive(Debug, Clone, Default)]
pub struct Stage {
    /// CTEs from this (sub)query's own `WITH` clause. Empty for the top-level body, whose
    /// `WITH` lives on [`Query`]; populated when a subquery carries its own `WITH`.
    pub ctes: Vec<Cte>,
    /// FROM clause; `None` for `SELECT 1` with no table.
    pub from: Option<From>,
    /// WHERE conjuncts (implicit AND between them).
    pub filter: Vec<Expr>,
    pub grouping: Option<Grouping>,
    /// Span of the `GROUP BY` keys (first key start … last key end) — lets the
    /// positional-reference rewrite locate an ordinal key. `None` when there's no `GROUP BY`.
    pub grouping_span: Option<Span>,
    /// HAVING conjuncts.
    pub having: Vec<Expr>,
    /// Span of the `HAVING` predicate (before AND-splitting) — used by the having→WHERE
    /// rewrite to relocate it.
    pub having_span: Option<Span>,
    pub windows: Vec<NamedWindow>,
    pub projection: Vec<ProjItem>,
    pub distinct: Distinct,
    pub ordering: Vec<OrderKey>,
    /// Span of the `ORDER BY` expressions — used by the order-by-drop rewrite to locate
    /// and delete the clause.
    pub ordering_span: Option<Span>,
    pub limit: Option<Expr>,
    /// Span of the `LIMIT` count expression — lets the `exists-with-limit` rewrite locate
    /// and delete the clause. `None` for `SELECT TOP`/`FETCH` forms (no `LIMIT` keyword).
    pub limit_span: Option<Span>,
    pub offset: Option<Expr>,
}

/// `UNION` / `INTERSECT` / `EXCEPT` combining two relations, with the ORDER BY /
/// LIMIT that apply to the combined result.
#[derive(Debug, Clone)]
pub struct SetOp {
    pub op: SetOpKind,
    pub quantifier: SetQuantifier,
    pub left: Relation,
    pub right: Relation,
    pub ordering: Vec<OrderKey>,
    pub ordering_span: Option<Span>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetQuantifier {
    All,
    Distinct,
}

/// The FROM clause as a join tree.
#[derive(Debug, Clone)]
pub enum From {
    Relation(RelationRef),
    Join(Box<Join>),
}

#[derive(Debug, Clone)]
pub struct Join {
    pub left: From,
    pub right: From,
    pub kind: JoinKind,
    pub constraint: JoinConstraint,
    /// Span of the join as the parser reports it — the right relation through the `ON`
    /// predicate (the `JOIN`/`INNER` keyword sits just before `start`). Lets the
    /// `filter-only-join` → `EXISTS` rewrite locate the fragment to splice out.
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub enum JoinConstraint {
    /// `ON <expr>`, with the span of the whole condition — lets the
    /// conditional-join→`UNION ALL` rewrite splice each arm in place of the `CASE`.
    On(Expr, Span),
    Using(Vec<Name>),
    Natural,
    /// No join condition (CROSS JOIN, or comma-join) — a cartesian product.
    None,
}

/// A reference to a relation in a FROM clause: a base table, a derived subquery, or a
/// table-valued function (`unnest(...)`, `generate_series(...)`, `jsonb_array_elements(...)`).
#[derive(Debug, Clone)]
pub enum RelationRef {
    BaseTable {
        name: TableName,
        alias: Option<Name>,
        span: Span,
        source_id: SourceId,
        binding: Option<SourceBinding>,
    },
    Derived {
        subquery: Box<Relation>,
        alias: Name,
        lateral: bool,
        source_id: SourceId,
    },
    /// A table-valued function in FROM. Its `args` are implicitly LATERAL — they may
    /// reference preceding FROM items — so a comma-join with one is not a Cartesian product.
    /// Output columns aren't modeled (treated as an unknown-schema source).
    TableFunction {
        name: Name,
        args: Vec<Expr>,
        alias: Option<Name>,
        source_id: SourceId,
    },
}

/// Schema information bound to a base table during schema-aware resolve (F4).
/// `None` under structural resolve.
#[derive(Debug, Clone)]
pub struct SourceBinding {
    pub columns: Vec<(String, Type)>,
}

#[derive(Debug, Clone)]
pub struct ProjItem {
    pub expr: Expr,
    pub alias: Option<Name>,
}

/// GROUP BY keys. GROUPING SETS / ROLLUP / CUBE are not modeled in v0.1 and surface
/// as `Opaque` expressions in `keys`.
#[derive(Debug, Clone)]
pub struct Grouping {
    pub keys: Vec<Expr>,
}

#[derive(Debug, Clone)]
pub struct OrderKey {
    pub expr: Expr,
    pub direction: Direction,
    pub nulls: NullsOrder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
    /// No explicit direction written.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullsOrder {
    First,
    Last,
    /// No explicit NULLS ordering written.
    Default,
}

/// `DISTINCT` / `DISTINCT ON (...)` / neither.
#[derive(Debug, Clone, Default)]
pub enum Distinct {
    #[default]
    No,
    All,
    On(Vec<Expr>),
}

/// A named window definition: `WINDOW w AS (PARTITION BY ...)`.
#[derive(Debug, Clone)]
pub struct NamedWindow {
    pub name: Name,
    pub spec: super::expr::WindowSpec,
}
