//! Data-modifying statements, modeled lean — enough for v0.1.5 safety rules
//! (e.g. "UPDATE/DELETE without WHERE") and reference validity. Richer detail is
//! added when a rule needs it.

use super::expr::Expr;
use super::name::{Name, Span, TableName};
use super::query::Query;
use super::stage::From;

#[derive(Debug, Clone)]
pub struct Delete {
    pub target: TableName,
    /// WHERE conjuncts. Empty means an unconditional DELETE.
    pub filter: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub target: TableName,
    pub assignments: Vec<Assignment>,
    /// Optional `UPDATE ... FROM` source.
    pub from: Option<From>,
    /// WHERE conjuncts. Empty means an unconditional UPDATE.
    pub filter: Vec<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub column: Name,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct Insert {
    pub target: TableName,
    pub columns: Vec<Name>,
    pub source: InsertSource,
}

#[derive(Debug, Clone)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}
