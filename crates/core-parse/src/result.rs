//! The common output type every analysis writes to, and its JSON wire format.
//!
//! A [`Finding`] is the unit of output. It carries two classification axes: [`Severity`]
//! (how bad — a neutral High/Medium/Low impact) and [`Category`] (what kind). Category is
//! *derived* from the stable `rule` id via [`Category::of`] — the single source of truth —
//! so rules don't set it and adding a category is a one-line table edit.

use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::model::name::Span;

/// How bad a finding is — a neutral impact scale that reads in every category (a `High`
/// performance hit, a `Low` maintainability smell). Ordered `Low < Medium < High`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

/// What *kind* of problem a finding is. Exactly one per finding, derived from the rule id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Malformed or references things that don't exist — won't run as written.
    Validity,
    /// Runs, but returns surprising/wrong results.
    Correctness,
    /// Correct results, but slow.
    Performance,
    /// Correct and fast, but fragile or unclear.
    Maintainability,
    /// Works in one dialect, surprises in another.
    Portability,
}

impl Category {
    /// The category for a rule id, or `None` if the rule isn't in the table. The single
    /// source of truth for the taxonomy; [`Finding::category`] applies the default.
    pub fn of(rule: &str) -> Option<Category> {
        use Category::*;
        Some(match rule {
            // Validity — the query (or schema) is malformed / references the nonexistent.
            "parse-error"
            | "unknown-column"
            | "unknown-table"
            | "ambiguous-column"
            | "ambiguous-table"
            | "unknown-table-alias"
            | "schema-ignored"
            | "group-by-aggregate" => Validity,

            // Correctness — runs, but wrong/surprising/dangerous results.
            "not-in-subquery"
            | "not-in-nullable-column"
            | "reversed-between"
            | "equals-null"
            | "not-equals-excludes-null"
            | "float-equality"
            | "inconsistent-parameter-type"
            | "integer-division"
            | "string-numeric-compare"
            | "like-on-numeric-column"
            | "self-comparison"
            | "count-nullable-column"
            | "select-non-grouped-column"
            | "is-null-on-not-null-column"
            | "not-in-list-with-null"
            | "group-by-constant"
            | "in-subquery-select-star"
            | "order-by-not-in-distinct-select"
            | "timestamp-compared-to-date"
            | "aggregate-in-where"
            | "mixed-aggregate-and-column"
            | "between-timestamp-date-bounds"
            | "redundant-nullif"
            | "implicit-boolean-int"
            | "limit-zero"
            | "like-all-wildcards"
            | "order-by-constant"
            | "risky-cast"
            | "join-type-mismatch"
            | "left-join-nullified"
            | "having-without-aggregate"
            | "limit-without-order-by"
            | "cartesian-join"
            | "unconditional-delete"
            | "unconditional-update"
            | "destructive-statement"
            | "dml-not-by-key"
            | "exists-with-aggregate"
            | "distinct-on-arbitrary-row"
            | "nested-aggregate"
            | "union-column-count-mismatch"
            | "where-references-select-alias"
            | "tautology-null-check"
            | "contradiction-null-check"
            | "in-subquery-with-limit"
            | "limit-in-derived-table-without-order"
            | "offset-without-order-by"
            | "window-in-where"
            | "distinct-in-window"
            | "window-rank-without-order"
            | "order-by-aggregate-without-group-by"
            | "insert-values-count-mismatch"
            | "update-column-to-self"
            | "having-count-zero"
            | "contradictory-predicates"
            | "sum-constant-for-count"
            | "tautological-or" => Correctness,

            // Performance — correct, but slow.
            "select-star"
            | "non-sargable-predicate"
            | "leading-wildcard-like"
            | "function-on-indexed-column"
            | "inequality-defeats-index"
            | "or-defeats-index"
            | "or-across-tables"
            | "index-prefix-mismatch"
            | "unindexed-filter"
            | "unindexed-join-key"
            | "offset-pagination"
            | "parameterized-like-pattern"
            | "parameterized-offset"
            | "deferred-join-pagination"
            | "offset-without-limit"
            | "order-by-random"
            | "large-in-list"
            | "excessive-joins"
            | "complex-subquery-in-where"
            | "correlated-subquery"
            | "scalar-subquery-in-select"
            | "repeated-filter-join"
            | "repeated-scalar-subquery"
            | "top-n-per-group"
            | "filter-only-join"
            | "count-star-vs-exists"
            | "redundant-distinct-in-subquery"
            | "implicit-cast-in-filter"
            | "union-vs-union-all"
            | "redundant-distinct-in-union"
            | "count-distinct-on-unique"
            | "select-distinct-on-unique"
            | "multiple-count-distinct"
            // join-`ON` shapes that block a hash/merge join → a slow plan.
            | "or-in-join-on"
            | "conditional-in-join-on"
            | "subquery-in-join-on"
            | "length-zero-comparison"
            | "select-distinct-single-aggregate"
            | "count-not-null-column"
            | "aggregated-derived-join"
            | "redundant-distinct-in-min-max" => Performance,

            // Maintainability — correct and fast, but fragile/unclear.
            "positional-reference"
            | "distinct-on-grouped"
            | "like-without-wildcard"
            | "redundant-or-chain"
            | "in-list-of-one"
            | "constant-predicate"
            | "order-by-in-subquery-without-limit"
            | "redundant-cast"
            | "redundant-else-null"
            | "case-to-coalesce"
            | "columns-in-exists"
            | "redundant-group-by-on-key"
            | "sum-case-to-count-filter"
            | "boolean-literal-comparison"
            | "redundant-coalesce-on-not-null"
            | "is-not-null-on-not-null-column"
            | "natural-join"
            | "redundant-subquery-in-from"
            | "double-negation"
            | "right-join"
            | "redundant-order-by-in-union-branch"
            | "nested-coalesce"
            | "case-when-boolean"
            | "coalesce-single-arg"
            | "in-list-with-duplicates"
            | "duplicate-column-in-select"
            | "duplicate-group-by-key"
            | "group-by-without-aggregate"
            | "distinct-star"
            | "count-constant-arg"
            | "insert-without-column-list"
            | "exists-with-limit"
            | "insert-select-star"
            | "negated-is-null"
            | "negated-in"
            | "negated-between"
            | "negated-like"
            | "redundant-null-in-coalesce"
            | "coalesce-identical-args"
            | "coalesce-dead-args-after-literal"
            | "redundant-coalesce-on-count"
            | "redundant-nested-function"
            | "case-to-nullif"
            | "redundant-is-not-null-guard"
            | "in-list-with-null"
            | "nested-concat"
            | "between-equal-bounds"
            | "greatest-least-single-arg"
            | "case-branches-identical"
            | "nested-case-in-else"
            | "duplicate-order-by-key"
            | "duplicate-predicate"
            | "redundant-boolean-literal-conjunct"
            | "group-by-pinned-column"
            | "unused-cte"
            | "unused-left-join" => Maintainability,

            // Portability — dialect-specific surprises.
            "pipe-operator-portability"
            | "plus-string-concat"
            | "count-distinct-multiple-columns"
            | "order-by-nullable-without-nulls"
            | "ilike-portability"
            | "ifnull-portability"
            | "isnull-to-coalesce"
            | "nonstandard-current-datetime" => Portability,

            _ => return None,
        })
    }

    /// The fallback for a rule with no table entry — neutral, so a new rule never panics in
    /// production. The coverage test fails CI if a rule actually emitted hits this.
    pub const DEFAULT: Category = Category::Maintainability;
}

/// One analysis finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable kebab-case id callers can filter on, e.g. `"unknown-column"`.
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    /// Source span of the offending construct — start + end, so a caller can highlight
    /// the region (not just point at it). Zero-width (start == end) for parse/resolve
    /// errors that only have a point.
    pub span: Option<Span>,
    /// Did-you-mean / quick hint.
    pub suggestion: Option<String>,
    /// The conditional "why" (filled by rules at 0.1.5).
    pub reasoning: Option<String>,
    /// Suggested rewritten SQL (filled by rewrites at 0.1.5).
    pub fix: Option<String>,
    /// The edits that turn the original SQL into [`fix`](Self::fix), each a range to
    /// replace — empty when there's no fix. A client applies these for a precise in-place
    /// change (LSP `TextEdit`-shaped); `fix` is the same change as a whole-statement string.
    pub edits: Vec<Edit>,
    /// The table/column this finding is about, for the `contextualize` pass to look up
    /// evidence (table volume, plan verdict) against. Internal scaffolding — never
    /// serialized, so it costs no wire format or tokenization. `None` on findings that
    /// don't opt into evidence re-scoring (see `docs/design-context-aware-findings.md`).
    #[serde(skip)]
    pub subject: Option<Subject>,
}

/// What a [`Finding`] is about, in normalized identifiers — enough for the `contextualize`
/// pass to key table statistics and an `EXPLAIN` plan verdict. `column` is `None` for
/// whole-relation findings (e.g. a cartesian join) that only carry a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub table: String,
    pub alias: Option<String>,
    pub column: Option<String>,
}

impl Finding {
    /// This finding's category, derived from its rule id (defaulting if unmapped).
    pub fn category(&self) -> Category {
        Category::of(&self.rule).unwrap_or(Category::DEFAULT)
    }
}

/// A single text replacement contributing to a [`Finding::fix`]: put `replacement` in
/// `range`. `replacement == ""` is a deletion; a zero-width `range` (start == end) is an
/// insertion. Applying a finding's edits (left positions stable, i.e. last-to-first) to the
/// original SQL reproduces its `fix`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: Span,
    pub replacement: String,
}

/// A schema recommendation: an index or schema change that would help the query. Unlike
/// a [`Finding`] (what's wrong), advice is *conditional* — a tree of scenarios the caller
/// matches against their own reality, since VARQ can't see table statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advice {
    /// What the advice is about, e.g. `"filter on orders.status"`.
    pub subject: String,
    /// Span of the predicate the advice concerns, for the caller to point at.
    pub span: Option<Span>,
    pub scenarios: Vec<Scenario>,
    /// How likely the index matters, judged from query *structure* alone (a join key that's the
    /// only path to its table outranks one filter among many). A priority signal, not a promise
    /// of a speedup — it's set exactly on do/don't-skip advice, where the enrich layer collapses
    /// the pair into one prioritized remedy (no row counts to decide the branches otherwise).
    /// `None` where the advisor offers real alternatives rather than a single do/don't (composite,
    /// top-N), which keeps its full scenario tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criticality: Option<Criticality>,
}

/// Structural priority of an index suggestion, used to rank and to collapse the do/don't
/// scenario pair into one prioritized remedy (whenever it's set — a schema alone gives no
/// row counts to decide the branches).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Criticality {
    High,
    Medium,
    Low,
}

impl Criticality {
    /// One notch less urgent — a range predicate benefits from an index less than equality does.
    /// `Low` is the floor.
    pub fn down(self) -> Self {
        match self {
            Self::High => Self::Medium,
            Self::Medium | Self::Low => Self::Low,
        }
    }
}

/// One branch of an [`Advice`] tree: when this applies, what to do, and the cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    /// When this recommendation is worth it, e.g. `"if the filter is selective (<5% of rows)"`.
    pub condition: String,
    pub recommendation: String,
    pub tradeoff: String,
}

/// The CI verdict for a result: does it block the build, warn, or pass? Computed from the
/// category-aware policy (see [`AnalysisResult::outcome`]). Ordered `Ok < Warn < Block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Ok,
    Warn,
    Block,
}

/// One bind parameter the query carries — its name and (when a schema let resolve infer it)
/// the type the caller must bind. A non-empty manifest means the query is a parameterized
/// *template*, not executable as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parameter {
    /// `$1`, `:name` — the parameter as the caller writes it.
    pub name: String,
    /// Inferred type name (`"integer"`, `"text"`, …), or `None` when it can't be inferred.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none", default)]
    pub ty: Option<String>,
    /// Every source position this parameter occurs at, so a surface can highlight each one
    /// (a reused `$1` carries more than one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
}

/// The result of analyzing one input: findings (what's wrong) and advice (what would
/// help), the bind parameters it carries, and the dialect it was analyzed under. The JSON
/// envelope (version + summary) is computed at serialization time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnalysisResult {
    pub findings: Vec<Finding>,
    pub advice: Vec<Advice>,
    /// Bind parameters present in the query — empty unless it's parameterized.
    pub parameters: Vec<Parameter>,
    pub dialect: Dialect,
    /// True when the advice is *hypothetical*: produced without a schema (the engine can't see
    /// which indexes already exist), so every recommendation is conditional. Set when analysis
    /// ran with no schema.
    pub advice_hypothetical: bool,
}

impl AnalysisResult {
    /// The highest severity present.
    pub fn max_severity(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }

    /// The category-aware CI verdict: the worst cell any finding hits. Only Validity and
    /// Correctness·High block; the advisory categories warn or pass — they never fail CI.
    pub fn outcome(&self) -> Outcome {
        self.findings
            .iter()
            .map(|f| cell(f.category(), f.severity))
            .max()
            .unwrap_or(Outcome::Ok)
    }

    /// Serialize to the documented `--json` envelope (built by the [`crate::enrich`] layer).
    pub fn to_json(&self) -> String {
        crate::enrich::to_json(self)
    }

    /// The rendered (enriched) view a front door consumes — short title, what/why, remedies.
    pub fn rendered(&self) -> crate::enrich::RenderedResult {
        crate::enrich::rendered(self)
    }
}

/// The category-aware exit policy (`docs/13`): worst cell wins.
pub(crate) fn cell(category: Category, severity: Severity) -> Outcome {
    use Category::*;
    use Severity::*;
    match (category, severity) {
        (Validity, _) => Outcome::Block,
        (Correctness, High) => Outcome::Block,
        (Correctness, _) => Outcome::Warn,
        (Performance | Portability, High | Medium) => Outcome::Warn,
        (Maintainability, High) => Outcome::Warn,
        _ => Outcome::Ok,
    }
}
