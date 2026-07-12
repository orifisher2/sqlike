//! VARQ's normalized representation of a SQL statement — the **stage model**.
//!
//! Raw sqlparser ASTs are input only; all analysis operates on this tree. Built in
//! two passes this phase: [`translate`] (AST → tree) and [`resolve`] (bind every
//! column reference). Schema-aware typing, predicate canonicalization, and the rest
//! of `04a-stage-model.md`'s pipeline are deferred to later phases / v0.4.

pub mod dml;
pub mod expr;
pub mod name;
pub mod query;
pub mod stage;
pub mod ty;

pub mod resolve;
pub mod tables;
pub mod translate;

pub use expr::{Binding, ColumnRef, Expr, Literal, PlaceholderKind, SourceId};
pub use name::{Name, Span, TableName};
pub use query::{Analyzed, Cte, Query};
pub use resolve::{
    resolve, resolve_suggestions, resolve_with, ResolveError, ResolveErrorKind, ResolvedQuery,
};
pub use stage::{From, Join, Relation, SetOp, Stage};
pub use tables::base_table_names;
pub use translate::translate;
pub use ty::Type;
