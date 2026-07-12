//! Scalar expressions — VARQ's own representation, not sqlparser's AST.
//!
//! Translate converts `ast::Expr` into this; anything VARQ doesn't model becomes
//! [`Expr::Opaque`] so analysis can decline rather than mis-read it.

use super::name::{Name, Span};
use super::stage::{OrderKey, Relation};
use super::ty::Type;

/// A stable identifier assigned to each FROM source during resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId(pub u32);

/// A reference to a column. After resolution `binding` (and, with schema, `ty`) is set.
#[derive(Debug, Clone)]
pub struct ColumnRef {
    /// Table qualifier as written: `u` in `u.email`.
    pub qualifier: Option<Name>,
    pub name: Name,
    pub span: Span,
    /// Filled by resolve; what this reference binds to.
    pub binding: Option<Binding>,
    /// Filled by schema-aware resolve (F4); `None` under structural resolve.
    pub ty: Option<Type>,
}

/// What a [`ColumnRef`] resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// A column provided by a FROM source (possibly in an enclosing scope —
    /// that is how correlated subqueries bind).
    Source { source: SourceId, column: String },
    /// An output column of the current stage, e.g. an ORDER BY referring to a
    /// SELECT-list alias.
    OutputAlias(String),
}

/// A literal value. Numbers keep their exact lexical form (precision-preserving,
/// `f64`-free, so the tree can derive equality/hash for v0.4 comparison later).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Literal {
    Number(String),
    Text(String),
    Bool(bool),
    Null,
    /// Typed literals: `DATE '2024-01-01'`, `INTERVAL '7 days'`, …
    Typed {
        ty: Type,
        raw: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    And,
    Or,
    Like,
    NotLike,
    ILike,
    Concat,
    /// An operator VARQ doesn't model individually; kept by symbol for display.
    Other(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Not,
    Minus,
    Plus,
    IsNull,
    IsNotNull,
}

/// The `OVER (...)` clause attached to a function call (makes it a window function).
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderKey>,
    // Frame clause (ROWS/RANGE BETWEEN …) deferred; recorded as Opaque if present.
}

/// A scalar expression.
#[derive(Debug, Clone)]
pub enum Expr {
    Column(ColumnRef),
    Literal(Literal),
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
        /// Span of the whole unary expression, so a rewrite can splice it.
        span: Span,
    },
    /// A function or aggregate call. Whether it is an aggregate or a window function
    /// is derived from `name` + `over` (see [`Expr::is_aggregate`]), not baked in.
    Function {
        name: Name,
        args: Vec<Expr>,
        distinct: bool,
        over: Option<WindowSpec>,
        /// Span of the whole call (`fn(...)`), so a rewrite can splice it.
        span: Span,
    },
    Cast {
        expr: Box<Expr>,
        ty: Type,
        /// Span of the whole cast construct — lets a rewrite locate and splice it.
        span: Span,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_branch: Option<Box<Expr>>,
        /// Span of the whole `CASE … END` — lets a rewrite locate and splice it.
        span: Span,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high` — an inclusive range test.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
        /// Span of the whole `expr [NOT] BETWEEN low AND high` — used by the `reversed-between`
        /// rewrite to splice the swapped bounds.
        span: Span,
    },
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Relation>,
        negated: bool,
        /// Span of the whole `expr [NOT] IN (subquery)` — used by rewrites to splice.
        span: Span,
    },
    Exists {
        subquery: Box<Relation>,
        negated: bool,
    },
    /// A scalar subquery used as a value: `(SELECT ...)`. The span covers the construct,
    /// so rewrites can splice it (e.g. `count-star-vs-exists` → `EXISTS`).
    ScalarSubquery(Box<Relation>, Span),
    /// `*` or `t.*` in a projection; expanded during analysis when schema is known.
    Wildcard {
        qualifier: Option<Name>,
        span: Span,
    },
    /// A bind parameter — `$1`, `:name`, or `?` (the last is rewritten to a positional
    /// `$N` before parsing, since the Postgres grammar reads a bare `?` as the jsonb
    /// operator). Unlike [`Expr::Opaque`] this is *understood* — "a value binds here" —
    /// so analysis proceeds instead of declining. `ty` is filled by schema-aware resolve
    /// (P2); `None` under structural resolve.
    Placeholder {
        kind: PlaceholderKind,
        ty: Option<Type>,
        /// Source position of the marker, so a surface can highlight each bind slot.
        span: Span,
    },
    /// A construct VARQ does not model in v0.1. Carries the original SQL so analysis
    /// declines gracefully (transparent-reasoning principle).
    Opaque {
        sql: String,
        span: Option<Span>,
    },
}

/// Which flavour of bind parameter a [`Expr::Placeholder`] is. A `?` is rewritten to a
/// fresh `$N` at parse time, so each `?` becomes a distinct `Positional` — which is exactly
/// the intended semantics (two `?`s are two parameters, one reused `$1` is one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderKind {
    /// `$1`, `$2`, … — the number is the parameter's identity.
    Positional(u32),
    /// `:name` / `$name` — the bare name, sigil stripped.
    Named(String),
}

/// Standard Postgres aggregate function names (normalized, lowercase).
const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "array_agg",
    "string_agg",
    "bool_and",
    "bool_or",
    "every",
    "json_agg",
    "jsonb_agg",
];

impl Expr {
    /// Whether this is an aggregate call *used as an aggregate* — a known aggregate
    /// name with no `OVER` clause. `SUM(x) OVER (...)` is a window function, not this.
    pub fn is_aggregate(&self) -> bool {
        matches!(
            self,
            Expr::Function { name, over: None, .. } if AGGREGATES.contains(&name.normalized().as_str())
        )
    }

    /// Whether this is a window function call (`... OVER (...)`).
    pub fn is_window(&self) -> bool {
        matches!(self, Expr::Function { over: Some(_), .. })
    }
}
