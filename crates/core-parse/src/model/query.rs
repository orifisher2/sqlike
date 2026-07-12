//! Top-level analyzed statements.

use super::dml::{Delete, Insert, Update};
use super::name::Name;
use super::stage::Relation;

/// The result of translating one parsed statement.
#[derive(Debug, Clone)]
pub enum Analyzed {
    Query(Query),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    /// DDL or any statement VARQ doesn't deeply model — analysis declines.
    Other {
        kind: String,
    },
    /// Expression nesting exceeded [`super::translate`]'s depth cap. Walking the tree would
    /// overflow the stack, so analysis declines with a single "too complex" finding.
    TooComplex,
}

/// A `SELECT` query: optional CTEs plus the body relation.
#[derive(Debug, Clone)]
pub struct Query {
    pub ctes: Vec<Cte>,
    pub body: Relation,
}

/// A `WITH` common table expression.
#[derive(Debug, Clone)]
pub struct Cte {
    pub name: Name,
    pub query: Relation,
    /// `WITH RECURSIVE` — represented but only best-effort analyzed (fixpoint is
    /// out of the finite-stage-tree model; see `04a-stage-model.md`).
    pub recursive: bool,
}
