//! Translate: sqlparser AST → VARQ's stage model.
//!
//! Mechanical and (almost) one-to-one. Constructs VARQ doesn't model become
//! [`Expr::Opaque`] / [`Analyzed::Other`] so downstream analysis declines rather
//! than mis-reads. Source ids are placeholders here; [`super::resolve`] assigns them.

use crate::parser::{ast, Location};
use sqlparser::ast::Spanned;
use sqlparser::tokenizer::{Location as SqlLoc, Span as SqlSpan};

use super::dml::{Assignment, Delete, Insert, InsertSource, Update};
use super::expr::{
    BinaryOp, ColumnRef, Expr, Literal, PlaceholderKind, SourceId, UnaryOp, WindowSpec,
};
use super::name::{Name, Span, TableName};
use super::query::{Analyzed, Cte, Query};
use super::stage::{
    Direction, Distinct, From, Grouping, Join, JoinConstraint, JoinKind, NamedWindow, NullsOrder,
    OrderKey, ProjItem, Relation, RelationRef, SetOp, SetOpKind, SetQuantifier, Stage,
};
use super::ty::Type;

const PLACEHOLDER_SOURCE: SourceId = SourceId(0);

/// Cap on expression nesting depth. A flat operator chain (`a OR a OR …`, `a + a + …`) parses
/// with O(1) parser recursion — so sqlparser's own recursion limit never trips — but builds a
/// linearly-deep AST. Translating it (and every downstream tree walk) recurses to that depth and
/// overflows the stack, which aborts the whole process: a crash-the-server DoS on a few-KB input.
/// Past this depth we stop and mark the statement too complex (see [`translate`]). 256 is far
/// beyond any real query (expressions nest well under 50 deep) yet safely under the stack budget.
const MAX_EXPR_DEPTH: u32 = 256;

thread_local! {
    static EXPR_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static DEPTH_EXCEEDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Restores the recursion-depth counter on every exit path from [`tr_expr`].
struct DepthGuard;
impl Drop for DepthGuard {
    fn drop(&mut self) {
        EXPR_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

/// Translate one parsed statement into the model.
pub fn translate(stmt: &ast::Statement) -> Analyzed {
    DEPTH_EXCEEDED.with(|f| f.set(false));
    let analyzed = match stmt {
        ast::Statement::Query(q) => Analyzed::Query(translate_query(q)),
        ast::Statement::Insert(i) => Analyzed::Insert(tr_insert(i)),
        ast::Statement::Update(u) => Analyzed::Update(tr_update(u)),
        ast::Statement::Delete(d) => Analyzed::Delete(tr_delete(d)),
        other => Analyzed::Other {
            kind: statement_kind(other),
        },
    };
    // A statement whose expression nesting blew past the cap can't be walked safely.
    if DEPTH_EXCEEDED.with(|f| f.get()) {
        return Analyzed::TooComplex;
    }
    analyzed
}

fn statement_kind(stmt: &ast::Statement) -> String {
    // The leading keyword is a stable-enough label for "a statement we don't model".
    stmt.to_string()
        .split_whitespace()
        .next()
        .unwrap_or("UNKNOWN")
        .to_uppercase()
}

// --- queries / relations -------------------------------------------------------

fn translate_query(q: &ast::Query) -> Query {
    let ctes = q.with.as_ref().map(translate_ctes).unwrap_or_default();
    Query {
        ctes,
        body: tr_query_body(q),
    }
}

/// A query used as a relation (subquery, set-op branch, CTE body). Its own ORDER BY / LIMIT
/// are attached to the produced node, and an inner `WITH` is kept on the produced `Stage`
/// (a `WITH` on a set-op-bodied subquery has no single `Stage` to hold it and is dropped).
fn tr_query_as_relation(q: &ast::Query) -> Relation {
    let mut rel = tr_query_body(q);
    if let (Some(with), Relation::Stage(stage)) = (&q.with, &mut rel) {
        stage.ctes = translate_ctes(with);
    }
    rel
}

fn translate_ctes(with: &ast::With) -> Vec<Cte> {
    with.cte_tables
        .iter()
        .map(|cte| Cte {
            name: name_of(&cte.alias.name),
            query: tr_query_as_relation(&cte.query),
            recursive: with.recursive,
        })
        .collect()
}

fn tr_query_body(q: &ast::Query) -> Relation {
    let mut rel = tr_relation(&q.body);
    attach_order_limit(&mut rel, q);
    rel
}

fn tr_relation(body: &ast::SetExpr) -> Relation {
    match body {
        ast::SetExpr::Select(sel) => Relation::Stage(Box::new(tr_select(sel))),
        ast::SetExpr::Query(inner) => tr_query_body(inner),
        ast::SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => Relation::SetOp(Box::new(SetOp {
            op: match op {
                ast::SetOperator::Union => SetOpKind::Union,
                ast::SetOperator::Intersect => SetOpKind::Intersect,
                ast::SetOperator::Except | ast::SetOperator::Minus => SetOpKind::Except,
            },
            quantifier: match set_quantifier {
                ast::SetQuantifier::All | ast::SetQuantifier::AllByName => SetQuantifier::All,
                _ => SetQuantifier::Distinct,
            },
            left: tr_relation(left),
            right: tr_relation(right),
            ordering: Vec::new(),
            ordering_span: None,
            limit: None,
            offset: None,
        })),
        // `VALUES (a, b), (c, d)` is a literal table — model it as its SQL-equivalent
        // `SELECT a, b UNION ALL SELECT c, d`, reusing the set-op machinery.
        ast::SetExpr::Values(v) if !v.rows.is_empty() => tr_values(v),
        // TABLE / embedded DML as a query body: not modeled structurally.
        other => Relation::Stage(Box::new(opaque_stage(&other.to_string()))),
    }
}

/// `VALUES (r1), (r2), …` → `SELECT r1 UNION ALL SELECT r2 UNION ALL …`, one single-row stage per
/// row, folded left-deep. Columns are named `EXPR$0, EXPR$1, …` (the generator convention these
/// benchmarks use); an explicit `AS t(a, b)` alias overrides them in [`tr_table_factor`].
fn tr_values(v: &ast::Values) -> Relation {
    let row_stage = |row: &ast::Parens<Vec<ast::Expr>>| {
        let projection = row
            .content
            .iter()
            .enumerate()
            .map(|(i, e)| ProjItem {
                expr: tr_expr(e),
                alias: Some(Name::new(format!("EXPR${i}"), false)),
            })
            .collect();
        Relation::Stage(Box::new(Stage {
            projection,
            ..Stage::default()
        }))
    };
    let mut it = v.rows.iter();
    let first = row_stage(it.next().expect("non-empty rows checked by caller"));
    it.fold(first, |left, row| {
        Relation::SetOp(Box::new(SetOp {
            op: SetOpKind::Union,
            quantifier: SetQuantifier::All,
            left,
            right: row_stage(row),
            ordering: Vec::new(),
            ordering_span: None,
            limit: None,
            offset: None,
        }))
    })
}

/// Apply a derived table's explicit column aliases (`… AS t(a, b)`) by renaming the output columns.
/// A set op takes its output names from the first branch, but every branch is renamed so a
/// multi-row `VALUES` exposes consistent column names on each row (needed once branches are compared
/// or distributed individually).
fn apply_alias_columns(rel: &mut Relation, cols: &[ast::TableAliasColumnDef]) {
    match rel {
        Relation::Stage(s) => {
            for (p, c) in s.projection.iter_mut().zip(cols) {
                p.alias = Some(name_of(&c.name));
            }
        }
        Relation::SetOp(op) => {
            apply_alias_columns(&mut op.left, cols);
            apply_alias_columns(&mut op.right, cols);
        }
    }
}

fn opaque_stage(sql: &str) -> Stage {
    Stage {
        projection: vec![ProjItem {
            expr: Expr::Opaque {
                sql: sql.to_string(),
                span: None,
            },
            alias: None,
        }],
        ..Stage::default()
    }
}

fn tr_select(sel: &ast::Select) -> Stage {
    let mut filter = Vec::new();
    if let Some(sel_expr) = &sel.selection {
        split_and(sel_expr, &mut filter);
    }
    let mut having = Vec::new();
    if let Some(h) = &sel.having {
        split_and(h, &mut having);
    }
    Stage {
        ctes: Vec::new(),
        from: tr_from(&sel.from),
        filter,
        grouping: tr_group_by(&sel.group_by),
        grouping_span: tr_group_by_span(&sel.group_by),
        having,
        having_span: sel.having.as_ref().map(|h| conv_span(h.span())),
        windows: tr_named_windows(&sel.named_window),
        projection: sel.projection.iter().map(tr_select_item).collect(),
        distinct: match &sel.distinct {
            None | Some(ast::Distinct::All) => Distinct::No,
            Some(ast::Distinct::Distinct) => Distinct::All,
            Some(ast::Distinct::On(exprs)) => Distinct::On(exprs.iter().map(tr_expr).collect()),
        },
        ordering: Vec::new(),
        ordering_span: None,
        // `SELECT TOP n` (T-SQL) is a row limit like `LIMIT`; model it so the limit-aware rules
        // see it. The outer query's `LIMIT`/`FETCH` (if any) takes precedence in attach_order_limit.
        limit: tr_top(sel.top.as_ref()),
        offset: None,
    }
}

/// `TOP n` / `TOP (expr)` (SQL Server) as a limit expression.
fn tr_top(top: Option<&ast::Top>) -> Option<Expr> {
    match top?.quantity.as_ref()? {
        ast::TopQuantity::Expr(e) => Some(tr_expr(e)),
        ast::TopQuantity::Constant(n) => Some(Expr::Literal(Literal::Number(n.to_string()))),
    }
}

fn attach_order_limit(rel: &mut Relation, q: &ast::Query) {
    let (ordering, ordering_span) = match &q.order_by {
        Some(ast::OrderBy {
            kind: ast::OrderByKind::Expressions(exprs),
            ..
        }) => {
            let span = exprs.first().zip(exprs.last()).map(|(f, l)| {
                Span::new(conv_loc(f.expr.span().start), conv_loc(l.expr.span().end))
            });
            (exprs.iter().map(tr_order_key).collect(), span)
        }
        _ => (Vec::new(), None),
    };
    let (limit, offset) = match &q.limit_clause {
        Some(ast::LimitClause::LimitOffset { limit, offset, .. }) => (
            limit.as_ref().map(tr_expr),
            offset.as_ref().map(|o| tr_expr(&o.value)),
        ),
        Some(ast::LimitClause::OffsetCommaLimit { offset, limit }) => {
            (Some(tr_expr(limit)), Some(tr_expr(offset)))
        }
        None => (None, None),
    };
    // `OFFSET … FETCH NEXT n ROWS ONLY` (T-SQL / standard) is a row limit too — its OFFSET lands
    // in `limit_clause` above, but the count is in `q.fetch`. Fall back to it when there's no LIMIT.
    let limit = limit.or_else(|| {
        q.fetch
            .as_ref()
            .and_then(|f| f.quantity.as_ref())
            .map(tr_expr)
    });
    match rel {
        // `.or(s.limit.take())` keeps a `SELECT TOP n` already set by tr_select when the outer
        // query has no LIMIT/FETCH (they can't both apply to one select).
        Relation::Stage(s) => {
            s.ordering = ordering;
            s.ordering_span = ordering_span;
            s.limit = limit.or(s.limit.take());
            s.offset = offset;
        }
        Relation::SetOp(so) => {
            so.ordering = ordering;
            so.ordering_span = ordering_span;
            so.limit = limit;
            so.offset = offset;
        }
    }
}

fn tr_group_by(g: &ast::GroupByExpr) -> Option<Grouping> {
    match g {
        ast::GroupByExpr::Expressions(exprs, _) if !exprs.is_empty() => Some(Grouping {
            keys: exprs.iter().map(tr_expr).collect(),
        }),
        _ => None,
    }
}

/// Span of the `GROUP BY` keys (first key start … last key end), mirroring `ordering_span`.
fn tr_group_by_span(g: &ast::GroupByExpr) -> Option<Span> {
    let ast::GroupByExpr::Expressions(exprs, _) = g else {
        return None;
    };
    exprs
        .first()
        .zip(exprs.last())
        .map(|(f, l)| Span::new(conv_loc(f.span().start), conv_loc(l.span().end)))
}

fn tr_named_windows(windows: &[ast::NamedWindowDefinition]) -> Vec<NamedWindow> {
    windows
        .iter()
        .filter_map(|nw| match &nw.1 {
            ast::NamedWindowExpr::WindowSpec(ws) => Some(NamedWindow {
                name: name_of(&nw.0),
                spec: tr_window_spec(ws),
            }),
            ast::NamedWindowExpr::NamedWindow(_) => None,
        })
        .collect()
}

fn tr_select_item(item: &ast::SelectItem) -> ProjItem {
    match item {
        ast::SelectItem::UnnamedExpr(e) => ProjItem {
            expr: tr_expr(e),
            alias: None,
        },
        ast::SelectItem::ExprWithAlias { expr, alias } => ProjItem {
            expr: tr_expr(expr),
            alias: Some(name_of(alias)),
        },
        ast::SelectItem::ExprWithAliases { expr, aliases } => ProjItem {
            expr: tr_expr(expr),
            alias: aliases.first().map(name_of),
        },
        ast::SelectItem::QualifiedWildcard(kind, opts) => ProjItem {
            expr: Expr::Wildcard {
                qualifier: qualified_wildcard_name(kind),
                span: conv_span(opts.wildcard_token.0.span),
            },
            alias: None,
        },
        ast::SelectItem::Wildcard(opts) => ProjItem {
            expr: Expr::Wildcard {
                qualifier: None,
                span: conv_span(opts.wildcard_token.0.span),
            },
            alias: None,
        },
    }
}

fn qualified_wildcard_name(kind: &ast::SelectItemQualifiedWildcardKind) -> Option<Name> {
    match kind {
        ast::SelectItemQualifiedWildcardKind::ObjectName(name) => Some(object_name_last(name)),
        ast::SelectItemQualifiedWildcardKind::Expr(_) => None,
    }
}

// --- FROM / joins --------------------------------------------------------------

fn tr_from(tables: &[ast::TableWithJoins]) -> Option<From> {
    let mut iter = tables.iter();
    let mut acc = tr_table_with_joins(iter.next()?);
    // Comma-separated FROM items are cross joins.
    for twj in iter {
        acc = From::Join(Box::new(Join {
            left: acc,
            right: tr_table_with_joins(twj),
            kind: JoinKind::Cross,
            constraint: JoinConstraint::None,
            span: conv_span(twj.span()),
        }));
    }
    Some(acc)
}

fn tr_table_with_joins(twj: &ast::TableWithJoins) -> From {
    let mut acc = tr_table_factor(&twj.relation);
    for join in &twj.joins {
        let (kind, constraint) = tr_join_operator(&join.join_operator);
        acc = From::Join(Box::new(Join {
            left: acc,
            right: tr_table_factor(&join.relation),
            kind,
            constraint,
            span: conv_span(join.span()),
        }));
    }
    acc
}

fn tr_join_operator(op: &ast::JoinOperator) -> (JoinKind, JoinConstraint) {
    use ast::JoinOperator as J;
    match op {
        J::Inner(c) | J::Join(c) => (JoinKind::Inner, tr_constraint(c)),
        J::Left(c) | J::LeftOuter(c) => (JoinKind::Left, tr_constraint(c)),
        J::Right(c) | J::RightOuter(c) => (JoinKind::Right, tr_constraint(c)),
        J::FullOuter(c) => (JoinKind::Full, tr_constraint(c)),
        J::CrossJoin(_) => (JoinKind::Cross, JoinConstraint::None),
        // Semi/anti/apply/asof and other non-standard joins: best-effort as inner.
        other => (JoinKind::Inner, join_operator_constraint(other)),
    }
}

fn join_operator_constraint(op: &ast::JoinOperator) -> JoinConstraint {
    use ast::JoinOperator as J;
    match op {
        J::Semi(c)
        | J::LeftSemi(c)
        | J::RightSemi(c)
        | J::Anti(c)
        | J::LeftAnti(c)
        | J::RightAnti(c)
        | J::StraightJoin(c) => tr_constraint(c),
        _ => JoinConstraint::None,
    }
}

fn tr_constraint(c: &ast::JoinConstraint) -> JoinConstraint {
    match c {
        ast::JoinConstraint::On(e) => JoinConstraint::On(tr_expr(e), conv_span(e.span())),
        ast::JoinConstraint::Using(names) => {
            JoinConstraint::Using(names.iter().map(object_name_last).collect())
        }
        ast::JoinConstraint::Natural => JoinConstraint::Natural,
        ast::JoinConstraint::None => JoinConstraint::None,
    }
}

fn tr_table_factor(tf: &ast::TableFactor) -> From {
    match tf {
        // `name(args) AS a` is a table-valued function; a bare `name` is a base table.
        ast::TableFactor::Table {
            name,
            alias,
            args: Some(targs),
            ..
        } => From::Relation(RelationRef::TableFunction {
            name: object_name_last(name),
            args: targs.args.iter().map(tr_function_arg).collect(),
            alias: alias.as_ref().map(|a| name_of(&a.name)),
            source_id: PLACEHOLDER_SOURCE,
        }),
        ast::TableFactor::Table { name, alias, .. } => From::Relation(RelationRef::BaseTable {
            name: object_name_to_table(name),
            alias: alias.as_ref().map(|a| name_of(&a.name)),
            span: object_name_span(name),
            source_id: PLACEHOLDER_SOURCE,
            binding: None,
        }),
        ast::TableFactor::Function {
            name, args, alias, ..
        } => From::Relation(RelationRef::TableFunction {
            name: object_name_last(name),
            args: args.iter().map(tr_function_arg).collect(),
            alias: alias.as_ref().map(|a| name_of(&a.name)),
            source_id: PLACEHOLDER_SOURCE,
        }),
        ast::TableFactor::UNNEST {
            array_exprs, alias, ..
        } => From::Relation(RelationRef::TableFunction {
            name: Name::new("unnest", false),
            args: array_exprs.iter().map(tr_expr).collect(),
            alias: alias.as_ref().map(|a| name_of(&a.name)),
            source_id: PLACEHOLDER_SOURCE,
        }),
        ast::TableFactor::Derived {
            lateral,
            subquery,
            alias,
            ..
        } => {
            let mut subrel = tr_query_as_relation(subquery);
            if let Some(a) = alias {
                if !a.columns.is_empty() {
                    apply_alias_columns(&mut subrel, &a.columns);
                }
            }
            From::Relation(RelationRef::Derived {
                subquery: Box::new(subrel),
                alias: alias
                    .as_ref()
                    .map(|a| name_of(&a.name))
                    .unwrap_or_else(|| Name::new("_derived", false)),
                lateral: *lateral,
                source_id: PLACEHOLDER_SOURCE,
            })
        }
        ast::TableFactor::NestedJoin {
            table_with_joins, ..
        } => tr_table_with_joins(table_with_joins),
        // Table functions, UNNEST, PIVOT, etc.: not modeled; keep the text as a name.
        other => From::Relation(RelationRef::BaseTable {
            name: TableName {
                schema: None,
                name: Name::new(other.to_string(), false),
            },
            alias: None,
            span: zero_span(),
            source_id: PLACEHOLDER_SOURCE,
            binding: None,
        }),
    }
}

// --- expressions ---------------------------------------------------------------

fn tr_expr(e: &ast::Expr) -> Expr {
    let depth = EXPR_DEPTH.with(|d| {
        let v = d.get() + 1;
        d.set(v);
        v
    });
    let _guard = DepthGuard;
    if depth > MAX_EXPR_DEPTH {
        // Bail with an inert node — and do NOT render `e` (its `Display` recurses over the whole
        // deep subtree, the very overflow we're avoiding). The statement is discarded upstream.
        DEPTH_EXCEEDED.with(|f| f.set(true));
        return Expr::Opaque {
            sql: String::new(),
            span: None,
        };
    }
    match e {
        ast::Expr::Identifier(ident) => Expr::Column(ColumnRef {
            qualifier: None,
            name: name_of(ident),
            span: ident_span(ident),
            binding: None,
            ty: None,
        }),
        ast::Expr::CompoundIdentifier(idents) => {
            let len = idents.len();
            let name = &idents[len - 1];
            Expr::Column(ColumnRef {
                qualifier: (len >= 2).then(|| name_of(&idents[len - 2])),
                name: name_of(name),
                span: ident_span(name),
                binding: None,
                ty: None,
            })
        }
        ast::Expr::Value(v) => tr_value(&v.value, conv_span(v.span)),
        ast::Expr::Nested(inner) => tr_expr(inner),
        ast::Expr::IsNull(inner) => unary(UnaryOp::IsNull, inner, conv_span(e.span())),
        ast::Expr::IsNotNull(inner) => unary(UnaryOp::IsNotNull, inner, conv_span(e.span())),
        ast::Expr::BinaryOp { left, op, right } => match map_binop(op) {
            Some(op) => Expr::Binary {
                op,
                left: Box::new(tr_expr(left)),
                right: Box::new(tr_expr(right)),
            },
            None => opaque(e),
        },
        ast::Expr::UnaryOp { op, expr } => match map_unop(op) {
            Some(op) => Expr::Unary {
                op,
                expr: Box::new(tr_expr(expr)),
                span: conv_span(e.span()),
            },
            None => opaque(e),
        },
        ast::Expr::Like {
            negated,
            expr,
            pattern,
            ..
        } => Expr::Binary {
            op: if *negated {
                BinaryOp::NotLike
            } else {
                BinaryOp::Like
            },
            left: Box::new(tr_expr(expr)),
            right: Box::new(tr_expr(pattern)),
        },
        ast::Expr::ILike {
            negated: false,
            expr,
            pattern,
            ..
        } => Expr::Binary {
            op: BinaryOp::ILike,
            left: Box::new(tr_expr(expr)),
            right: Box::new(tr_expr(pattern)),
        },
        ast::Expr::Cast {
            expr, data_type, ..
        } => Expr::Cast {
            expr: Box::new(tr_expr(expr)),
            ty: Type::from_ast(data_type),
            span: conv_span(e.span()),
        },
        ast::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => Expr::Case {
            operand: operand.as_ref().map(|o| Box::new(tr_expr(o))),
            whens: conditions
                .iter()
                .map(|cw| (tr_expr(&cw.condition), tr_expr(&cw.result)))
                .collect(),
            else_branch: else_result.as_ref().map(|e| Box::new(tr_expr(e))),
            span: conv_span(e.span()),
        },
        ast::Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(tr_expr(expr)),
            list: list.iter().map(tr_expr).collect(),
            negated: *negated,
        },
        ast::Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(tr_expr(expr)),
            low: Box::new(tr_expr(low)),
            high: Box::new(tr_expr(high)),
            negated: *negated,
            span: conv_span(e.span()),
        },
        ast::Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(tr_expr(expr)),
            subquery: Box::new(tr_query_as_relation(subquery)),
            negated: *negated,
            span: conv_span(e.span()),
        },
        ast::Expr::Exists { subquery, negated } => Expr::Exists {
            subquery: Box::new(tr_query_as_relation(subquery)),
            negated: *negated,
        },
        ast::Expr::Subquery(q) => {
            Expr::ScalarSubquery(Box::new(tr_query_as_relation(q)), conv_span(e.span()))
        }
        ast::Expr::Function(func) => tr_function(func),
        ast::Expr::Wildcard(tok) => Expr::Wildcard {
            qualifier: None,
            span: conv_span(tok.0.span),
        },
        ast::Expr::QualifiedWildcard(name, tok) => Expr::Wildcard {
            qualifier: Some(object_name_last(name)),
            span: conv_span(tok.0.span),
        },
        // `ts AT TIME ZONE zone` is exactly Postgres's `timezone(zone, ts)`; lower it so the
        // sargability/function rules see the wrapped column instead of an opaque blob.
        ast::Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => Expr::Function {
            name: Name::new("timezone", false),
            args: vec![tr_expr(time_zone), tr_expr(timestamp)],
            distinct: false,
            over: None,
            span: conv_span(e.span()),
        },
        _ => opaque(e),
    }
}

fn unary(op: UnaryOp, inner: &ast::Expr, span: Span) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(tr_expr(inner)),
        span,
    }
}

fn opaque(e: &ast::Expr) -> Expr {
    Expr::Opaque {
        sql: e.to_string(),
        span: None,
    }
}

fn tr_value(v: &ast::Value, span: Span) -> Expr {
    match v {
        ast::Value::Number(s, _) => Expr::Literal(Literal::Number(s.clone())),
        ast::Value::SingleQuotedString(s) | ast::Value::DoubleQuotedString(s) => {
            Expr::Literal(Literal::Text(s.clone()))
        }
        ast::Value::Boolean(b) => Expr::Literal(Literal::Bool(*b)),
        ast::Value::Null => Expr::Literal(Literal::Null),
        ast::Value::Placeholder(p) => Expr::Placeholder {
            kind: classify_placeholder(p),
            ty: None,
            span,
        },
        other => Expr::Opaque {
            sql: other.to_string(),
            span: None,
        },
    }
}

/// `$1` → positional, `:name` / `$name` → named (sigil stripped). A bare `?` never reaches
/// here — the parser pre-pass rewrites it to a positional `$N` first.
fn classify_placeholder(p: &str) -> PlaceholderKind {
    if let Some(rest) = p.strip_prefix('$') {
        if let Ok(n) = rest.parse::<u32>() {
            return PlaceholderKind::Positional(n);
        }
        return PlaceholderKind::Named(rest.to_string());
    }
    PlaceholderKind::Named(p.strip_prefix(':').unwrap_or(p).to_string())
}

fn map_binop(op: &ast::BinaryOperator) -> Option<BinaryOp> {
    use ast::BinaryOperator as B;
    Some(match op {
        B::Eq => BinaryOp::Eq,
        B::NotEq => BinaryOp::NotEq,
        B::Lt => BinaryOp::Lt,
        B::LtEq => BinaryOp::LtEq,
        B::Gt => BinaryOp::Gt,
        B::GtEq => BinaryOp::GtEq,
        B::Plus => BinaryOp::Plus,
        B::Minus => BinaryOp::Minus,
        B::Multiply => BinaryOp::Multiply,
        B::Divide => BinaryOp::Divide,
        B::Modulo => BinaryOp::Modulo,
        B::And => BinaryOp::And,
        B::Or => BinaryOp::Or,
        B::StringConcat => BinaryOp::Concat,
        _ => return None,
    })
}

fn map_unop(op: &ast::UnaryOperator) -> Option<UnaryOp> {
    use ast::UnaryOperator as U;
    Some(match op {
        U::Not => UnaryOp::Not,
        U::Minus => UnaryOp::Minus,
        U::Plus => UnaryOp::Plus,
        _ => return None,
    })
}

fn tr_function(func: &ast::Function) -> Expr {
    let (args, distinct) = match &func.args {
        ast::FunctionArguments::None => (Vec::new(), false),
        ast::FunctionArguments::Subquery(q) => (
            vec![Expr::ScalarSubquery(
                Box::new(tr_query_as_relation(q)),
                conv_span(q.span()),
            )],
            false,
        ),
        ast::FunctionArguments::List(list) => {
            let distinct = matches!(
                list.duplicate_treatment,
                Some(ast::DuplicateTreatment::Distinct)
            );
            (list.args.iter().map(tr_function_arg).collect(), distinct)
        }
    };
    let over = func.over.as_ref().map(|w| match w {
        ast::WindowType::WindowSpec(ws) => tr_window_spec(ws),
        ast::WindowType::NamedWindow(_) => WindowSpec {
            partition_by: Vec::new(),
            order_by: Vec::new(),
        },
    });
    Expr::Function {
        name: object_name_last(&func.name),
        args,
        distinct,
        over,
        span: conv_span(func.span()),
    }
}

fn tr_function_arg(arg: &ast::FunctionArg) -> Expr {
    let fae = match arg {
        ast::FunctionArg::Unnamed(fae)
        | ast::FunctionArg::Named { arg: fae, .. }
        | ast::FunctionArg::ExprNamed { arg: fae, .. } => fae,
    };
    match fae {
        ast::FunctionArgExpr::Expr(e) => tr_expr(e),
        ast::FunctionArgExpr::QualifiedWildcard(name) => Expr::Wildcard {
            qualifier: Some(object_name_last(name)),
            span: zero_span(),
        },
        ast::FunctionArgExpr::WildcardWithOptions(opts) => Expr::Wildcard {
            qualifier: None,
            span: conv_span(opts.wildcard_token.0.span),
        },
        ast::FunctionArgExpr::Wildcard => Expr::Wildcard {
            qualifier: None,
            span: zero_span(),
        },
    }
}

fn tr_window_spec(ws: &ast::WindowSpec) -> WindowSpec {
    WindowSpec {
        partition_by: ws.partition_by.iter().map(tr_expr).collect(),
        order_by: ws.order_by.iter().map(tr_order_key).collect(),
    }
}

fn tr_order_key(obe: &ast::OrderByExpr) -> OrderKey {
    OrderKey {
        expr: tr_expr(&obe.expr),
        direction: match obe.options.asc {
            Some(true) => Direction::Asc,
            Some(false) => Direction::Desc,
            None => Direction::Default,
        },
        nulls: match obe.options.nulls_first {
            Some(true) => NullsOrder::First,
            Some(false) => NullsOrder::Last,
            None => NullsOrder::Default,
        },
    }
}

// --- DML -----------------------------------------------------------------------

fn tr_delete(d: &ast::Delete) -> Delete {
    let tables = match &d.from {
        ast::FromTable::WithFromKeyword(t) | ast::FromTable::WithoutKeyword(t) => t,
    };
    let (target, span) = tables
        .first()
        .map(|twj| table_factor_target(&twj.relation))
        .unwrap_or_else(|| (unknown_table(), zero_span()));
    let mut filter = Vec::new();
    if let Some(sel) = &d.selection {
        split_and(sel, &mut filter);
    }
    Delete {
        target,
        filter,
        span,
    }
}

fn tr_update(u: &ast::Update) -> Update {
    let (target, span) = table_factor_target(&u.table.relation);
    let mut filter = Vec::new();
    if let Some(sel) = &u.selection {
        split_and(sel, &mut filter);
    }
    Update {
        target,
        assignments: u
            .assignments
            .iter()
            .map(|a| Assignment {
                column: assignment_target_name(&a.target),
                value: tr_expr(&a.value),
            })
            .collect(),
        from: None,
        filter,
        span,
    }
}

fn tr_insert(i: &ast::Insert) -> Insert {
    let target = match &i.table {
        ast::TableObject::TableName(name) => object_name_to_table(name),
        other => TableName {
            schema: None,
            name: Name::new(other.to_string(), false),
        },
    };
    let source = match &i.source {
        Some(q) => match &*q.body {
            ast::SetExpr::Values(values) => InsertSource::Values(
                values
                    .rows
                    .iter()
                    .map(|row| row.iter().map(tr_expr).collect())
                    .collect(),
            ),
            _ => InsertSource::Query(Box::new(translate_query(q))),
        },
        None => InsertSource::Values(Vec::new()),
    };
    Insert {
        target,
        columns: i.columns.iter().map(object_name_last).collect(),
        source,
    }
}

fn table_factor_target(tf: &ast::TableFactor) -> (TableName, Span) {
    match tf {
        ast::TableFactor::Table { name, .. } => {
            (object_name_to_table(name), object_name_span(name))
        }
        other => (
            TableName {
                schema: None,
                name: Name::new(other.to_string(), false),
            },
            zero_span(),
        ),
    }
}

fn assignment_target_name(t: &ast::AssignmentTarget) -> Name {
    match t {
        ast::AssignmentTarget::ColumnName(name) => object_name_last(name),
        ast::AssignmentTarget::Tuple(names) => names
            .first()
            .map(object_name_last)
            .unwrap_or_else(unknown_name),
    }
}

// --- helpers -------------------------------------------------------------------

/// Split a boolean expression on top-level `AND` into its conjuncts.
fn split_and(e: &ast::Expr, out: &mut Vec<Expr>) {
    if let ast::Expr::BinaryOp {
        left,
        op: ast::BinaryOperator::And,
        right,
    } = e
    {
        split_and(left, out);
        split_and(right, out);
    } else {
        out.push(tr_expr(e));
    }
}

fn name_of(ident: &ast::Ident) -> Name {
    Name::new(ident.value.clone(), ident.quote_style.is_some())
}

fn object_name_idents(name: &ast::ObjectName) -> Vec<&ast::Ident> {
    name.0.iter().filter_map(|p| p.as_ident()).collect()
}

fn object_name_last(name: &ast::ObjectName) -> Name {
    object_name_idents(name)
        .last()
        .map(|i| name_of(i))
        .unwrap_or_else(|| Name::new(name.to_string(), false))
}

fn object_name_to_table(name: &ast::ObjectName) -> TableName {
    let idents = object_name_idents(name);
    match idents.len() {
        0 => TableName {
            schema: None,
            name: Name::new(name.to_string(), false),
        },
        1 => TableName {
            schema: None,
            name: name_of(idents[0]),
        },
        n => TableName {
            schema: Some(name_of(idents[n - 2])),
            name: name_of(idents[n - 1]),
        },
    }
}

fn object_name_span(name: &ast::ObjectName) -> Span {
    object_name_idents(name)
        .last()
        .map(|i| ident_span(i))
        .unwrap_or_else(zero_span)
}

fn unknown_table() -> TableName {
    TableName {
        schema: None,
        name: unknown_name(),
    }
}

fn unknown_name() -> Name {
    Name::new("«unknown»", false)
}

fn ident_span(ident: &ast::Ident) -> Span {
    conv_span(ident.span)
}

fn conv_span(s: SqlSpan) -> Span {
    Span::new(conv_loc(s.start), conv_loc(s.end))
}

fn conv_loc(l: SqlLoc) -> Location {
    Location {
        line: l.line,
        column: l.column,
    }
}

fn zero_span() -> Span {
    let z = Location { line: 0, column: 0 };
    Span::new(z, z)
}

#[cfg(test)]
mod tests {
    use super::{classify_placeholder, PlaceholderKind};

    #[test]
    fn classifies_positional() {
        assert_eq!(classify_placeholder("$1"), PlaceholderKind::Positional(1));
        assert_eq!(classify_placeholder("$42"), PlaceholderKind::Positional(42));
    }

    #[test]
    fn classifies_named() {
        assert_eq!(
            classify_placeholder(":name"),
            PlaceholderKind::Named("name".to_string())
        );
        assert_eq!(
            classify_placeholder("$name"),
            PlaceholderKind::Named("name".to_string())
        );
    }
}
