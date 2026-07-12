//! Resolution — binds every column reference to the FROM source that provides it,
//! using a scope stack so correlated subqueries and LATERAL bind to enclosing
//! sources. With a [`Schema`] it is additionally *schema-aware*: it checks column
//! existence, fills types, and flags typos.
//!
//! Soundness over completeness: a check only fires when it can be certain. If a
//! source's columns aren't known (a CTE, a derived subquery), references that might
//! belong to it are left unbound rather than flagged. See `docs/phase-4.md`.

use std::collections::HashSet;

use crate::dialect::Dialect;
use crate::parser::{parse, Location};
use crate::schema::Schema;

use super::expr::{BinaryOp, Binding, ColumnRef, Expr, SourceId};
use super::query::{Analyzed, Query};
use super::stage::{From, JoinConstraint, Relation, RelationRef, SourceBinding, Stage};
use super::translate::translate;
use super::ty::Type;

/// Real-name typo suggestions for a query's unresolved references, by source location.
///
/// The tokenizing client recomputes these locally (Decision 6, `docs/14`): the server only
/// sees opaque tokens, so every reference looks like `vqtN` and its `closest()` is meaningless.
/// This reruns exactly the `parse → translate → resolve` the engine does, but keeps only each
/// resolve error's real-name suggestion paired with where it occurred.
pub fn resolve_suggestions(
    sql: &str,
    schema_ddl: Option<&str>,
    dialect: Dialect,
) -> Vec<(Location, String)> {
    let schema = schema_ddl.and_then(|ddl| Schema::from_ddl_with(ddl, dialect).ok());
    let Ok(statements) = parse(sql, dialect) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for stmt in &statements {
        if let Analyzed::Query(q) = translate(stmt) {
            if let Err(errors) = resolve_with(q, schema.as_ref(), dialect) {
                out.extend(
                    errors
                        .into_iter()
                        .filter_map(|e| e.suggestion.map(|s| (e.location, s))),
                );
            }
        }
    }
    out
}

/// A query whose references have been resolved. Only constructible here, so analysis
/// modules can only ever be handed a resolved tree.
#[derive(Debug)]
pub struct ResolvedQuery(Query);

impl ResolvedQuery {
    pub fn query(&self) -> &Query {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveErrorKind {
    UnknownQualifier,
    AmbiguousQualifier,
    UnknownTable,
    UnknownColumn,
    AmbiguousColumn,
}

/// A reference that could not be resolved. `suggestion` is the closest in-scope name,
/// for typos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    pub kind: ResolveErrorKind,
    pub message: String,
    pub location: Location,
    pub suggestion: Option<String>,
}

/// Resolve a query. With `schema = None` this is structural only; with a schema it
/// also checks column/table existence and fills types. Returns the resolved tree, or
/// every unresolved reference found.
/// Resolve under Postgres. Shim over [`resolve_with`] — keeps existing callers terse.
pub fn resolve(query: Query, schema: Option<&Schema>) -> Result<ResolvedQuery, Vec<ResolveError>> {
    resolve_with(query, schema, Dialect::Postgres)
}

/// Resolve under `dialect` — needed for dialect-specific names (e.g. SQLite's `rowid`).
pub fn resolve_with(
    mut query: Query,
    schema: Option<&Schema>,
    dialect: Dialect,
) -> Result<ResolvedQuery, Vec<ResolveError>> {
    let mut r = Resolver {
        next_id: 0,
        errors: Vec::new(),
        schema,
        dialect,
        cte_names: HashSet::new(),
    };
    r.resolve_query(&mut query, &[]);
    if r.errors.is_empty() {
        Ok(ResolvedQuery(query))
    } else {
        Err(r.errors)
    }
}

/// A FROM source visible during resolution.
#[derive(Clone)]
struct SourceEntry {
    id: SourceId,
    /// Normalized alias, or table name when no alias.
    name: Option<String>,
    /// `Some` for a base table found in the schema (normalized column name + type);
    /// `None` when the column set isn't known (CTE, derived subquery, no schema).
    columns: Option<Vec<(String, Type)>>,
}

#[derive(Clone, Default)]
struct Scope {
    sources: Vec<SourceEntry>,
    /// Projection aliases of the owning stage (for ORDER BY / HAVING alias refs).
    aliases: Vec<String>,
    /// Column names merged by a `USING`/`NATURAL` join — an unqualified reference to one is
    /// the single coalesced column, so it is not ambiguous across the joined sources.
    merged: HashSet<String>,
}

struct Resolver<'a> {
    next_id: u32,
    errors: Vec<ResolveError>,
    schema: Option<&'a Schema>,
    dialect: Dialect,
    /// Names introduced by `WITH` clauses; such a FROM reference is a CTE, not a table.
    cte_names: HashSet<String>,
}

impl Resolver<'_> {
    /// SQLite's implicit row-id pseudo-columns, valid on any ordinary table. `column` is
    /// already normalized (lower-cased).
    fn is_sqlite_rowid(&self, column: &str) -> bool {
        self.dialect == Dialect::Sqlite && matches!(column, "rowid" | "_rowid_" | "oid")
    }

    fn fresh_id(&mut self) -> SourceId {
        let id = SourceId(self.next_id);
        self.next_id += 1;
        id
    }

    fn resolve_query(&mut self, q: &mut Query, outer: &[Scope]) {
        for cte in &q.ctes {
            self.cte_names.insert(cte.name.normalized());
        }
        for cte in &mut q.ctes {
            self.resolve_relation(&mut cte.query, outer);
        }
        self.resolve_relation(&mut q.body, outer);
    }

    fn resolve_relation(&mut self, rel: &mut Relation, outer: &[Scope]) {
        match rel {
            Relation::Stage(s) => self.resolve_stage(s, outer),
            Relation::SetOp(so) => {
                self.resolve_relation(&mut so.left, outer);
                self.resolve_relation(&mut so.right, outer);
                for k in &mut so.ordering {
                    self.resolve_expr(&mut k.expr, outer);
                }
            }
        }
    }

    fn resolve_stage(&mut self, stage: &mut Stage, outer: &[Scope]) {
        // A subquery's own `WITH`: register its names (so `FROM cte` isn't an unknown table)
        // and resolve the bodies against the enclosing scope, like the top-level CTEs.
        for cte in &stage.ctes {
            self.cte_names.insert(cte.name.normalized());
        }
        for cte in &mut stage.ctes {
            self.resolve_relation(&mut cte.query, outer);
        }

        let mut sources = Vec::new();
        if let Some(from) = &mut stage.from {
            self.collect_sources(from, outer, &mut sources);
        }
        let aliases = stage
            .projection
            .iter()
            .filter_map(|p| p.alias.as_ref().map(|a| a.normalized()))
            .collect();
        let merged = stage
            .from
            .as_ref()
            .map(|from| merged_columns(from, &sources))
            .unwrap_or_default();

        let mut scopes = outer.to_vec();
        scopes.push(Scope {
            sources,
            aliases,
            merged,
        });

        if let Some(from) = &mut stage.from {
            self.resolve_join_constraints(from, &scopes);
        }
        for e in &mut stage.filter {
            self.resolve_expr(e, &scopes);
        }
        if let Some(g) = &mut stage.grouping {
            for k in &mut g.keys {
                self.resolve_expr(k, &scopes);
            }
        }
        for e in &mut stage.having {
            self.resolve_expr(e, &scopes);
        }
        for p in &mut stage.projection {
            self.resolve_expr(&mut p.expr, &scopes);
        }
        for w in &mut stage.windows {
            self.resolve_window(&mut w.spec, &scopes);
        }
        for k in &mut stage.ordering {
            self.resolve_expr(&mut k.expr, &scopes);
        }
        if let Some(e) = &mut stage.limit {
            self.resolve_expr(e, &scopes);
        }
        if let Some(e) = &mut stage.offset {
            self.resolve_expr(e, &scopes);
        }
    }

    fn collect_sources(
        &mut self,
        from: &mut From,
        outer: &[Scope],
        sources: &mut Vec<SourceEntry>,
    ) {
        match from {
            From::Relation(rr) => match rr {
                RelationRef::BaseTable {
                    name,
                    alias,
                    source_id,
                    binding,
                    span,
                } => {
                    let id = self.fresh_id();
                    *source_id = id;
                    let norm = name.name.normalized();
                    let columns = self.lookup_table_columns(&norm, name, *span, binding);
                    let visible = alias
                        .as_ref()
                        .map(|a| a.normalized())
                        .unwrap_or_else(|| norm.clone());
                    sources.push(SourceEntry {
                        id,
                        name: Some(visible),
                        columns,
                    });
                }
                RelationRef::Derived {
                    subquery,
                    alias,
                    lateral,
                    source_id,
                } => {
                    let id = self.fresh_id();
                    *source_id = id;
                    let mut sub_outer = outer.to_vec();
                    if *lateral {
                        sub_outer.push(Scope {
                            sources: sources.clone(),
                            ..Default::default()
                        });
                    }
                    self.resolve_relation(subquery, &sub_outer);
                    sources.push(SourceEntry {
                        id,
                        name: Some(alias.normalized()),
                        columns: None, // derived output columns not enumerated in v0.1
                    });
                }
                RelationRef::TableFunction {
                    name,
                    args,
                    alias,
                    source_id,
                } => {
                    let id = self.fresh_id();
                    *source_id = id;
                    // Table-function args are implicitly LATERAL — resolve them against the
                    // siblings collected so far (plus the outer scope).
                    let mut arg_scopes = outer.to_vec();
                    arg_scopes.push(Scope {
                        sources: sources.clone(),
                        ..Default::default()
                    });
                    for arg in args.iter_mut() {
                        self.resolve_expr(arg, &arg_scopes);
                    }
                    let visible = alias
                        .as_ref()
                        .map(|a| a.normalized())
                        .unwrap_or_else(|| name.normalized());
                    sources.push(SourceEntry {
                        id,
                        name: Some(visible),
                        columns: None, // function output columns not modeled
                    });
                }
            },
            From::Join(j) => {
                self.collect_sources(&mut j.left, outer, sources);
                self.collect_sources(&mut j.right, outer, sources);
            }
        }
    }

    /// Resolve a base-table reference against the schema, filling its `binding`.
    /// Returns the column set for the source entry (`None` if the columns aren't known).
    fn lookup_table_columns(
        &mut self,
        norm: &str,
        name: &super::name::TableName,
        span: super::name::Span,
        binding: &mut Option<SourceBinding>,
    ) -> Option<Vec<(String, Type)>> {
        if self.cte_names.contains(norm) {
            return None; // a CTE reference, not a schema table
        }
        let schema = self.schema?;
        match schema.table(&name.name) {
            Some(table) => {
                let cols: Vec<(String, Type)> = table
                    .columns
                    .iter()
                    .map(|c| (c.name.normalized(), c.ty.clone()))
                    .collect();
                *binding = Some(SourceBinding {
                    columns: cols.clone(),
                });
                Some(cols)
            }
            None => {
                let suggestion = closest(norm, schema.table_names());
                self.errors.push(ResolveError {
                    kind: ResolveErrorKind::UnknownTable,
                    message: format!("unknown table `{}`", name.name.text),
                    location: span.start,
                    suggestion,
                });
                None
            }
        }
    }

    fn resolve_join_constraints(&mut self, from: &mut From, scopes: &[Scope]) {
        if let From::Join(j) = from {
            self.resolve_join_constraints(&mut j.left, scopes);
            self.resolve_join_constraints(&mut j.right, scopes);
            if let JoinConstraint::On(e, _) = &mut j.constraint {
                self.resolve_expr(e, scopes);
            }
        }
    }

    fn resolve_window(&mut self, spec: &mut super::expr::WindowSpec, scopes: &[Scope]) {
        for e in &mut spec.partition_by {
            self.resolve_expr(e, scopes);
        }
        for k in &mut spec.order_by {
            self.resolve_expr(&mut k.expr, scopes);
        }
    }

    fn resolve_expr(&mut self, e: &mut Expr, scopes: &[Scope]) {
        match e {
            Expr::Column(c) => self.resolve_column(c, scopes),
            Expr::Binary { op, left, right } => {
                self.resolve_expr(left, scopes);
                self.resolve_expr(right, scopes);
                if is_comparison(*op) {
                    infer_comparison(left, right);
                }
            }
            Expr::Unary { expr, .. } => self.resolve_expr(expr, scopes),
            Expr::Cast { expr, ty, .. } => {
                self.resolve_expr(expr, scopes);
                set_placeholder_type(expr, ty);
            }
            Expr::Function { args, over, .. } => {
                for a in args {
                    self.resolve_expr(a, scopes);
                }
                if let Some(w) = over {
                    self.resolve_window(w, scopes);
                }
            }
            Expr::Case {
                operand,
                whens,
                else_branch,
                ..
            } => {
                if let Some(o) = operand {
                    self.resolve_expr(o, scopes);
                }
                for (cond, res) in whens {
                    self.resolve_expr(cond, scopes);
                    self.resolve_expr(res, scopes);
                }
                if let Some(e) = else_branch {
                    self.resolve_expr(e, scopes);
                }
            }
            Expr::InList { expr, list, .. } => {
                self.resolve_expr(expr, scopes);
                for item in list.iter_mut() {
                    self.resolve_expr(item, scopes);
                }
                if let Some(ty) = column_type(expr) {
                    for item in list.iter_mut() {
                        set_placeholder_type(item, &ty);
                    }
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.resolve_expr(expr, scopes);
                self.resolve_expr(low, scopes);
                self.resolve_expr(high, scopes);
                if let Some(ty) = column_type(expr) {
                    set_placeholder_type(low, &ty);
                    set_placeholder_type(high, &ty);
                }
            }
            Expr::InSubquery { expr, subquery, .. } => {
                self.resolve_expr(expr, scopes);
                self.resolve_relation(subquery, scopes);
            }
            Expr::Exists { subquery, .. } => self.resolve_relation(subquery, scopes),
            Expr::ScalarSubquery(sub, _) => self.resolve_relation(sub, scopes),
            Expr::Literal(_)
            | Expr::Placeholder { .. }
            | Expr::Wildcard { .. }
            | Expr::Opaque { .. } => {}
        }
    }

    fn resolve_column(&mut self, col: &mut ColumnRef, scopes: &[Scope]) {
        let column = col.name.normalized();
        match col.qualifier.clone() {
            Some(q) => self.resolve_qualified(col, &q.normalized(), &column, scopes),
            None => self.resolve_unqualified(col, &column, scopes),
        }
    }

    fn resolve_qualified(&mut self, col: &mut ColumnRef, q: &str, column: &str, scopes: &[Scope]) {
        for scope in scopes.iter().rev() {
            let mut matches = scope
                .sources
                .iter()
                .filter(|s| s.name.as_deref() == Some(q));
            let Some(entry) = matches.next() else {
                continue;
            };
            if matches.next().is_some() {
                self.push(
                    col,
                    ResolveErrorKind::AmbiguousQualifier,
                    format!("ambiguous table reference `{q}`"),
                    None,
                );
                return;
            }
            match &entry.columns {
                Some(cols) => match cols.iter().find(|(n, _)| n == column) {
                    Some((_, ty)) => {
                        col.ty = Some(ty.clone());
                        col.binding = Some(Binding::Source {
                            source: entry.id,
                            column: column.to_string(),
                        });
                    }
                    None if self.is_sqlite_rowid(column) => {
                        // SQLite rowid pseudo-column: valid on this table, bind structurally.
                        col.binding = Some(Binding::Source {
                            source: entry.id,
                            column: column.to_string(),
                        });
                    }
                    None => {
                        let sugg = closest(column, cols.iter().map(|(n, _)| n.clone()).collect());
                        self.push(
                            col,
                            ResolveErrorKind::UnknownColumn,
                            format!("column `{column}` does not exist on `{q}`"),
                            sugg,
                        );
                    }
                },
                None => {
                    // Unknown-schema source (CTE/derived): bind structurally, no check.
                    col.binding = Some(Binding::Source {
                        source: entry.id,
                        column: column.to_string(),
                    });
                }
            }
            return;
        }
        self.push(
            col,
            ResolveErrorKind::UnknownQualifier,
            format!("unknown table or alias `{q}`"),
            None,
        );
    }

    /// An unqualified column binds to the innermost scope that owns it, walking
    /// outward so correlated subqueries reach an enclosing query's columns (the
    /// qualified path already does this). Only the absence of the name from *every*
    /// scope is an error — mirroring `resolve_qualified`.
    fn resolve_unqualified(&mut self, col: &mut ColumnRef, column: &str, scopes: &[Scope]) {
        // The innermost scope that proved the name absent — used for the error's
        // suggestion if no enclosing scope owns it either.
        let mut absent_in: Option<&Scope> = None;
        for (depth, scope) in scopes.iter().rev().enumerate() {
            if depth == 0 && scope.aliases.iter().any(|a| a == column) {
                col.binding = Some(Binding::OutputAlias(column.to_string()));
                return;
            }
            if scope.sources.is_empty() {
                continue;
            }
            match self.try_bind_unqualified(col, column, scope) {
                BindOutcome::Bound | BindOutcome::Ambiguous | BindOutcome::Unprovable => return,
                BindOutcome::Absent => {
                    absent_in.get_or_insert(scope);
                }
            }
        }
        if let Some(scope) = absent_in {
            // An unqualified SQLite rowid is valid on any table; leave it unbound, no error.
            if self.is_sqlite_rowid(column) {
                return;
            }
            let candidates: Vec<String> = scope
                .sources
                .iter()
                .filter_map(|s| s.columns.as_ref())
                .flat_map(|cols| cols.iter().map(|(n, _)| n.clone()))
                .collect();
            if !candidates.is_empty() {
                let sugg = closest(column, candidates);
                self.push(
                    col,
                    ResolveErrorKind::UnknownColumn,
                    format!("column `{column}` does not exist"),
                    sugg,
                );
            }
        }
    }

    /// Try to bind `column` within a single scope. `Absent` means every source here
    /// is known and none owns it (safe to try an enclosing scope); `Unprovable` means
    /// an unknown-schema source might own it (stop, but don't error).
    fn try_bind_unqualified(
        &mut self,
        col: &mut ColumnRef,
        column: &str,
        scope: &Scope,
    ) -> BindOutcome {
        if scope.sources.len() == 1 {
            let s = &scope.sources[0];
            return match &s.columns {
                // Unknown-schema source (CTE/derived): bind structurally, no check.
                None => {
                    col.binding = Some(Binding::Source {
                        source: s.id,
                        column: column.to_string(),
                    });
                    BindOutcome::Bound
                }
                Some(cols) => match cols.iter().find(|(n, _)| n == column) {
                    Some((_, ty)) => {
                        col.ty = Some(ty.clone());
                        col.binding = Some(Binding::Source {
                            source: s.id,
                            column: column.to_string(),
                        });
                        BindOutcome::Bound
                    }
                    None => BindOutcome::Absent,
                },
            };
        }

        let has_unknown = scope.sources.iter().any(|s| s.columns.is_none());
        let mut owners = scope.sources.iter().filter_map(|s| {
            s.columns
                .as_ref()?
                .iter()
                .find(|(n, _)| n == column)
                .map(|(_, ty)| (s.id, ty.clone()))
        });
        match (owners.next(), owners.next()) {
            (Some((id, ty)), None) => {
                col.ty = Some(ty);
                col.binding = Some(Binding::Source {
                    source: id,
                    column: column.to_string(),
                });
                BindOutcome::Bound
            }
            // A `USING`/`NATURAL` join merges its column into one — not ambiguous. Bind to the
            // first owner (they share the coalesced value and type).
            (Some((id, ty)), Some(_)) if scope.merged.contains(column) => {
                col.ty = Some(ty);
                col.binding = Some(Binding::Source {
                    source: id,
                    column: column.to_string(),
                });
                BindOutcome::Bound
            }
            (Some(_), Some(_)) => {
                self.push(
                    col,
                    ResolveErrorKind::AmbiguousColumn,
                    format!("ambiguous column `{column}`"),
                    None,
                );
                BindOutcome::Ambiguous
            }
            // Sound: only "absent" (try an enclosing scope) when every source is known.
            (None, _) if has_unknown => BindOutcome::Unprovable,
            (None, _) => BindOutcome::Absent,
        }
    }

    fn push(
        &mut self,
        col: &ColumnRef,
        kind: ResolveErrorKind,
        message: String,
        suggestion: Option<String>,
    ) {
        self.errors.push(ResolveError {
            kind,
            message,
            location: col.span.start,
            suggestion,
        });
    }
}

/// Outcome of trying to bind an unqualified column within one scope.
enum BindOutcome {
    /// Bound to a source (a known column, or a structural bind to an unknown-schema source).
    Bound,
    /// Bound to nothing because the name is owned by ≥2 sources here; already reported.
    Ambiguous,
    /// Every source here is known and none owns the name — try an enclosing scope.
    Absent,
    /// An unknown-schema source here might own the name; stop without erroring.
    Unprovable,
}

/// Column names merged into a single output column by a `USING`/`NATURAL` join in this FROM
/// tree. An unqualified reference to one is the coalesced column, not an ambiguity.
fn merged_columns(from: &From, sources: &[SourceEntry]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_merged(from, sources, &mut out);
    out
}

fn collect_merged(from: &From, sources: &[SourceEntry], out: &mut HashSet<String>) {
    let From::Join(j) = from else {
        return;
    };
    collect_merged(&j.left, sources, out);
    collect_merged(&j.right, sources, out);
    match &j.constraint {
        JoinConstraint::Using(names) => out.extend(names.iter().map(|n| n.normalized())),
        // NATURAL merges every column common to the two sides (known columns only).
        JoinConstraint::Natural => {
            let left = columns_under(&j.left, sources);
            let right = columns_under(&j.right, sources);
            out.extend(left.intersection(&right).cloned());
        }
        _ => {}
    }
}

/// Normalized names of the known columns of every source under a FROM subtree.
fn columns_under(from: &From, sources: &[SourceEntry]) -> HashSet<String> {
    let mut ids = HashSet::new();
    source_ids_under(from, &mut ids);
    sources
        .iter()
        .filter(|s| ids.contains(&s.id))
        .filter_map(|s| s.columns.as_ref())
        .flat_map(|cols| cols.iter().map(|(n, _)| n.clone()))
        .collect()
}

fn source_ids_under(from: &From, out: &mut HashSet<SourceId>) {
    match from {
        From::Relation(
            RelationRef::BaseTable { source_id, .. }
            | RelationRef::Derived { source_id, .. }
            | RelationRef::TableFunction { source_id, .. },
        ) => {
            out.insert(*source_id);
        }
        From::Join(j) => {
            source_ids_under(&j.left, out);
            source_ids_under(&j.right, out);
        }
    }
}

// --- placeholder type inference (P2) --------------------------------------------
//
// A bind parameter takes the type of the typed operand directly beside it, so a
// schema-aware analysis (and later, advice) knows what the caller must bind. Local
// only: a bare resolved `Column` is the type anchor — no function/arithmetic results,
// no transitive propagation.

fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    )
}

/// The type of `e` when it is a resolved column carrying one (`None` without a schema).
fn column_type(e: &Expr) -> Option<Type> {
    match e {
        Expr::Column(c) => c.ty.clone(),
        _ => None,
    }
}

/// Give an as-yet-untyped placeholder `e` the inferred type `ty`.
fn set_placeholder_type(e: &mut Expr, ty: &Type) {
    if let Expr::Placeholder { ty: slot, .. } = e {
        if slot.is_none() {
            *slot = Some(ty.clone());
        }
    }
}

/// `col = ?` / `? = col`: the placeholder side takes the column side's type.
fn infer_comparison(left: &mut Expr, right: &mut Expr) {
    if let Some(ty) = column_type(left) {
        set_placeholder_type(right, &ty);
    } else if let Some(ty) = column_type(right) {
        set_placeholder_type(left, &ty);
    }
}

/// The candidate closest to `target` by edit distance, within a small threshold.
fn closest(target: &str, candidates: Vec<String>) -> Option<String> {
    let target = target.to_lowercase();
    candidates
        .into_iter()
        .map(|c| {
            let d = levenshtein(&target, &c.to_lowercase());
            (d, c)
        })
        .filter(|(d, _)| *d > 0 && *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}
