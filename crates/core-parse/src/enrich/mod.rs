//! The presentation layer: turn a detection-level [`Finding`]/[`Advice`] into the rich wire shape
//! the front doors render (a short `title`, split `what`/`why`, structured `remedies`).
//!
//! Derived at the boundary, never stored on `Finding`, exactly like [`Category::of`] (`result.rs`).
//! Content is **dialect-aware**: a per-dialect module ([`postgres`]/[`mysql`]/[`sqlite`]/[`mssql`])
//! supplies the copy for rules whose text or example genuinely differs on that engine; everything
//! else comes from [`common`]; anything not hand-written falls back to [`derive`]. Adding a dialect
//! is a new file plus one arm in [`dialect_rich`]; it never touches another dialect.

mod common;
mod mssql;
mod mysql;
mod postgres;
mod sqlite;

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;
use crate::model::name::Span;
use crate::result::{AnalysisResult, Category, Criticality, Edit, Finding, Parameter, Severity};

/// A way to fix a finding, or a schema change an advisor recommends: one shape for both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remedy {
    pub title: String,
    pub explanation: String,
    pub how_to_implement: String,
    pub why_it_solves: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub example: Option<String>,
    /// The condition gate: `Some` for conditional/advisory remedies.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub when: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tradeoff: Option<String>,
    /// Auto-applicable form (today's `fix`/`edits`); `None` means advisory only.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub apply: Option<Apply>,
}

/// The machine-applicable form of a remedy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Apply {
    pub fixed_sql: String,
    pub edits: Vec<Edit>,
}

/// A finding as rendered: detection facts (`rule`/`severity`/`category`/`span`) plus the rich
/// presentation (`title`/`what`/`why`/`remedies`). This is the whole wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingWire {
    pub rule: String,
    pub severity: Severity,
    pub category: Category,
    pub span: Option<Span>,
    pub title: String,
    pub what: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub why: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub remedies: Vec<Remedy>,
}

/// Advice as rendered: a subject, its span, and the same `Remedy` shape as findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdviceWire {
    pub subject: String,
    pub span: Option<Span>,
    pub remedies: Vec<Remedy>,
    /// Hypothetical advice: produced without a schema, so it's conditional (the engine can't tell
    /// whether the index already exists). Front doors label and group it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hypothetical: bool,
    /// Structural priority, set only on collapsed hypothetical advice (where we replace the
    /// do/don't scenario pair with one prioritized remedy). Front doors show it as a badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criticality: Option<Criticality>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A single ordering key for the findings list. [`RenderedResult::sort`] takes an ordered list of
/// these (primary first, each a tiebreak for the previous); [`DEFAULT_SORT`] is the default chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FindingSort {
    /// Worst first: High, then Medium, then Low.
    #[default]
    Severity,
    /// By category: validity, correctness, performance, maintainability, portability.
    Category,
    /// Query order: by span (line, then column); span-less findings (e.g. parse errors) first.
    Location,
}

/// The default issue order: worst first, then grouped by type, then in query order. Applied by
/// [`rendered`].
pub const DEFAULT_SORT: &[FindingSort] = &[
    FindingSort::Severity,
    FindingSort::Category,
    FindingSort::Location,
];

/// The rendered result a front door consumes: the deserialize target for a remote envelope and the
/// render input for a local analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RenderedResult {
    #[serde(default)]
    pub dialect: Dialect,
    pub findings: Vec<FindingWire>,
    #[serde(default)]
    pub advice: Vec<AdviceWire>,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
}

impl RenderedResult {
    /// Serialize to the documented `--json` envelope (version + summary + findings/advice).
    pub fn to_json(&self) -> String {
        envelope_json(self)
    }

    /// Order the findings by `keys` (primary first; each later key breaks ties of the previous).
    /// A stable sort, so findings equal on every key keep their relative order.
    pub fn sort(&mut self, keys: &[FindingSort]) {
        self.findings.sort_by(|a, b| {
            for key in keys {
                let ord = match key {
                    FindingSort::Severity => {
                        severity_rank(a.severity).cmp(&severity_rank(b.severity))
                    }
                    FindingSort::Category => {
                        category_rank(a.category).cmp(&category_rank(b.category))
                    }
                    FindingSort::Location => pos_key(&a.span).cmp(&pos_key(&b.span)),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
    }

    /// The category-aware CI verdict, computed from the findings' category + severity (the same
    /// policy as [`AnalysisResult::outcome`]), so a remote result exits with the same code.
    pub fn outcome(&self) -> crate::result::Outcome {
        self.findings
            .iter()
            .map(|f| crate::result::cell(f.category, f.severity))
            .max()
            .unwrap_or(crate::result::Outcome::Ok)
    }
}

/// Sort rank for severity: High first.
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::High => 0,
        Severity::Medium => 1,
        Severity::Low => 2,
    }
}

/// Sort rank for category: the triage order.
fn category_rank(c: Category) -> u8 {
    match c {
        Category::Validity => 0,
        Category::Correctness => 1,
        Category::Performance => 2,
        Category::Maintainability => 3,
        Category::Portability => 4,
    }
}

/// Position key for sorting: `(line, column)` of the span start; span-less findings sort first.
fn pos_key(span: &Option<Span>) -> (u64, u64) {
    span.map(|s| (s.start.line, s.start.column))
        .unwrap_or((0, 0))
}

/// The rich parts derived for a finding.
struct Parts {
    title: String,
    what: String,
    why: String,
    remedies: Vec<Remedy>,
}

/// Enrich one finding into its wire shape, under `dialect`.
pub fn enrich(f: &Finding, dialect: Dialect) -> FindingWire {
    let p = dialect_rich(f, dialect)
        .or_else(|| common::rich(f))
        .unwrap_or_else(|| derive(f));
    FindingWire {
        rule: f.rule.clone(),
        severity: f.severity,
        category: f.category(),
        span: f.span,
        title: p.title,
        what: p.what,
        why: p.why,
        remedies: p.remedies,
    }
}

/// Per-dialect copy for the rules that diverge on this engine, or `None` to use [`common`].
fn dialect_rich(f: &Finding, dialect: Dialect) -> Option<Parts> {
    match dialect {
        Dialect::Postgres => postgres::rich(f),
        Dialect::Mysql => mysql::rich(f),
        Dialect::Sqlite => sqlite::rich(f),
        Dialect::Mssql => mssql::rich(f),
    }
}

/// Enrich one advice into its wire shape. Advice carrying a `criticality` is a do/don't-skip pair
/// (index it / leave it unindexed): without table statistics the two branches can't be told apart,
/// so printing both reads as self-contradiction — collapse it into one prioritized remedy (the
/// index) with the skip branch folded into a caveat, and surface the `criticality` as a badge. This
/// holds whether or not a schema was supplied — a schema alone gives no row counts. Advice with no
/// `criticality` offers genuine alternatives (composite vs. single-column, top-N, …), so it keeps
/// its full scenario tree.
pub fn enrich_advice(a: &crate::result::Advice, hypothetical: bool) -> AdviceWire {
    let why = "Lets the planner seek straight to the rows via the index instead of scanning the \
               table.";
    let scenario_remedy = |s: &crate::result::Scenario| Remedy {
        title: format!("Index for {}", a.subject),
        explanation: s.condition.clone(),
        how_to_implement: s.recommendation.clone(),
        why_it_solves: why.into(),
        example: Some(s.recommendation.clone()),
        when: Some(s.condition.clone()),
        tradeoff: Some(s.tradeoff.clone()),
        apply: None,
    };

    match (a.criticality, a.scenarios.as_slice()) {
        // Collapse the do/don't pair: the first scenario is the index, the rest fold into a caveat.
        (Some(criticality), [action, rest @ ..]) => {
            let caveat = rest.iter().next().map_or_else(
                || action.tradeoff.clone(),
                |skip| format!("{}. Skip the index {}", action.tradeoff, skip.condition),
            );
            AdviceWire {
                subject: a.subject.clone(),
                span: a.span,
                remedies: vec![Remedy {
                    tradeoff: Some(caveat),
                    ..scenario_remedy(action)
                }],
                hypothetical,
                criticality: Some(criticality),
            }
        }
        _ => AdviceWire {
            subject: a.subject.clone(),
            span: a.span,
            remedies: a.scenarios.iter().map(scenario_remedy).collect(),
            hypothetical,
            criticality: None,
        },
    }
}

/// The full rendered view of a local analysis (the CLI's local path), findings in the default
/// ([`FindingSort::Severity`]) order.
pub fn rendered(result: &AnalysisResult) -> RenderedResult {
    let mut r = RenderedResult {
        dialect: result.dialect,
        findings: result
            .findings
            .iter()
            .map(|f| enrich(f, result.dialect))
            .collect(),
        advice: result
            .advice
            .iter()
            .map(|a| enrich_advice(a, result.advice_hypothetical))
            .collect(),
        parameters: result.parameters.clone(),
    };
    r.sort(DEFAULT_SORT);
    r
}

/// Serialize a local analysis to the documented `--json` envelope.
pub fn to_json(result: &AnalysisResult) -> String {
    envelope_json(&rendered(result))
}

/// Serialize an already-rendered result to the same envelope. The remote path uses this: the
/// client detokenizes the server's response into a [`RenderedResult`], then re-emits the envelope
/// for `--json` (`summary` recomputed from the findings, `version` the local crate's).
pub fn envelope_json(result: &RenderedResult) -> String {
    let mut summary = Summary::default();
    for f in &result.findings {
        match f.severity {
            Severity::High => summary.by_severity.high += 1,
            Severity::Medium => summary.by_severity.medium += 1,
            Severity::Low => summary.by_severity.low += 1,
        }
        match f.category {
            Category::Validity => summary.by_category.validity += 1,
            Category::Correctness => summary.by_category.correctness += 1,
            Category::Performance => summary.by_category.performance += 1,
            Category::Maintainability => summary.by_category.maintainability += 1,
            Category::Portability => summary.by_category.portability += 1,
        }
    }
    let report = Report {
        version: env!("CARGO_PKG_VERSION"),
        dialect: result.dialect,
        summary,
        findings: &result.findings,
        advice: &result.advice,
        parameters: result.parameters.clone(),
    };
    serde_json::to_string_pretty(&report).unwrap_or_default()
}

#[derive(Serialize)]
struct Report<'a> {
    version: &'static str,
    dialect: Dialect,
    summary: Summary,
    findings: &'a [FindingWire],
    advice: &'a [AdviceWire],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parameters: Vec<Parameter>,
}

#[derive(Serialize, Default)]
struct Summary {
    by_severity: SeverityCounts,
    by_category: CategoryCounts,
}

#[derive(Serialize, Default)]
struct SeverityCounts {
    high: usize,
    medium: usize,
    low: usize,
}

#[derive(Serialize, Default)]
struct CategoryCounts {
    validity: usize,
    correctness: usize,
    performance: usize,
    maintainability: usize,
    portability: usize,
}

// --- shared remedy builders, used by `common` and every dialect module ---

/// A static remedy with no auto-apply.
fn remedy(title: &str, explanation: &str, how: &str, why_solves: &str, example: &str) -> Remedy {
    Remedy {
        title: title.into(),
        explanation: explanation.into(),
        how_to_implement: how.into(),
        why_it_solves: why_solves.into(),
        example: Some(example.into()),
        when: None,
        tradeoff: None,
        apply: None,
    }
}

/// The auto-applicable rewrite a finding already carries (`fix`/`edits`), as a remedy. Falls back
/// to a static remedy with the same copy when the finding has no machine-applicable fix.
fn apply_remedy(
    f: &Finding,
    title: &str,
    explanation: &str,
    how: &str,
    why_solves: &str,
    example: &str,
) -> Remedy {
    match &f.fix {
        Some(fix) => Remedy {
            title: title.into(),
            explanation: explanation.into(),
            how_to_implement: how.into(),
            why_it_solves: why_solves.into(),
            example: Some(fix.clone()),
            when: None,
            tradeoff: None,
            apply: Some(Apply {
                fixed_sql: fix.clone(),
                edits: f.edits.clone(),
            }),
        },
        None => remedy(title, explanation, how, why_solves, example),
    }
}

/// The did-you-mean remedy for a resolve error, built from the nearest in-scope name. Shared with
/// [`finalize`](crate::tokenize::finalize), which rebuilds it client-side from the real names the
/// server never saw (it only had opaque tokens, so its "nearest" was meaningless, Decision 6).
pub(crate) fn resolve_remedy(suggestion: &str) -> Remedy {
    remedy(
        &format!("Did you mean `{suggestion}`?"),
        "The closest matching name that exists in scope.",
        &format!("Replace the unresolved name with `{suggestion}`."),
        "It resolves against the schema, so the query runs.",
        &format!("`{suggestion}`"),
    )
}

/// Fallback when a rule has no hand-written entry: a humanized title, `what` from the message,
/// `why` from the reasoning, and one remedy from the suggestion or the fix.
fn derive(f: &Finding) -> Parts {
    let mut remedies = Vec::new();
    if let Some(fix) = &f.fix {
        remedies.push(Remedy {
            title: "Apply the suggested rewrite".into(),
            explanation: "The flagged statement can be rewritten to avoid the issue.".into(),
            how_to_implement: "Replace the flagged statement with the rewrite below.".into(),
            why_it_solves: "The rewrite removes the flagged pattern while preserving the result."
                .into(),
            example: Some(fix.clone()),
            when: None,
            tradeoff: None,
            apply: Some(Apply {
                fixed_sql: fix.clone(),
                edits: f.edits.clone(),
            }),
        });
    } else if let Some(s) = &f.suggestion {
        remedies.push(Remedy {
            title: "Suggested change".into(),
            explanation: "A quick suggestion for this finding.".into(),
            how_to_implement: s.clone(),
            why_it_solves: "Addresses the flagged issue.".into(),
            example: None,
            when: None,
            tradeoff: None,
            apply: None,
        });
    }
    Parts {
        title: humanize(&f.rule),
        what: f.message.clone(),
        why: f.reasoning.clone().unwrap_or_default(),
        remedies,
    }
}

/// `"not-in-subquery"` becomes `"Not in subquery"`.
fn humanize(rule: &str) -> String {
    let mut s = rule.replace('-', " ");
    if let Some(c) = s.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(rule: &str, suggestion: Option<&str>) -> Finding {
        Finding {
            rule: rule.into(),
            severity: Severity::Low,
            message: "msg".into(),
            span: None,
            suggestion: suggestion.map(Into::into),
            reasoning: Some("because".into()),
            fix: None,
            edits: vec![],
            subject: None,
        }
    }

    #[test]
    fn unmapped_rule_falls_back_to_derivation() {
        let w = enrich(&finding("some-unmapped-rule", None), Dialect::Postgres);
        assert_eq!(w.title, "Some unmapped rule");
        assert_eq!(w.what, "msg");
        assert_eq!(w.why, "because");
    }

    #[test]
    fn typo_becomes_a_did_you_mean_remedy() {
        let w = enrich(&finding("unknown-column", Some("email")), Dialect::Postgres);
        assert!(
            w.remedies.iter().any(|r| r.title.contains("email")),
            "{:?}",
            w.remedies
        );
    }

    #[test]
    fn batch_rules_have_hand_written_titles_and_a_remedy() {
        let w = enrich(&finding("select-star", None), Dialect::Postgres);
        assert_ne!(w.title, "Select star", "select-star should be hand-written");
        assert!(w.remedies.iter().all(|r| r.example.is_some()));
        assert!(!w.remedies.is_empty());
    }

    #[test]
    fn divergent_rule_differs_by_dialect() {
        let pg = enrich(&finding("large-in-list", None), Dialect::Postgres);
        let my = enrich(&finding("large-in-list", None), Dialect::Mysql);
        assert_ne!(
            pg.remedies, my.remedies,
            "large-in-list should give dialect-specific advice"
        );
        assert!(pg
            .remedies
            .iter()
            .any(|r| r.example.as_deref().is_some_and(|e| e.contains("ANY"))));
    }

    #[test]
    fn correctness_batch_is_rich_and_diverges() {
        // A common correctness rule is hand-written, with an example on its remedy.
        let fe = enrich(&finding("float-equality", None), Dialect::Postgres);
        assert_ne!(fe.title, "Float equality");
        assert!(!fe.remedies.is_empty() && fe.remedies.iter().all(|r| r.example.is_some()));
        // A divergent one gives different advice per dialect.
        let pg = enrich(&finding("string-numeric-compare", None), Dialect::Postgres);
        let my = enrich(&finding("string-numeric-compare", None), Dialect::Mysql);
        assert_ne!(pg.why, my.why);
        assert!(pg.why.to_lowercase().contains("error"), "pg: {}", pg.why);
        assert!(my.why.to_lowercase().contains("scan"), "my: {}", my.why);
    }

    fn span(line: u64, column: u64) -> Span {
        let loc = crate::parser::Location { line, column };
        Span {
            start: loc,
            end: loc,
        }
    }

    #[test]
    fn default_sort_is_severity_then_position() {
        let mut res = AnalysisResult::default();
        let mut a = finding("select-star", None);
        a.severity = Severity::Low;
        a.span = Some(span(1, 1));
        let mut b = finding("not-in-subquery", None);
        b.severity = Severity::High;
        b.span = Some(span(5, 1));
        let mut c = finding("offset-pagination", None);
        c.severity = Severity::High;
        c.span = Some(span(2, 1));
        res.findings = vec![a, b, c];

        // Default chain [severity, type, location]: both High first, broken by category
        // (correctness `not-in-subquery` before performance `offset-pagination`), then Low a.
        let by_default = rendered(&res);
        let rules: Vec<&str> = by_default
            .findings
            .iter()
            .map(|f| f.rule.as_str())
            .collect();
        assert_eq!(
            rules,
            ["not-in-subquery", "offset-pagination", "select-star"]
        );

        // By location only: line 1, 2, 5.
        let mut r = rendered(&res);
        r.sort(&[FindingSort::Location]);
        let rules: Vec<&str> = r.findings.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(
            rules,
            ["select-star", "offset-pagination", "not-in-subquery"]
        );
    }

    #[test]
    fn v0_3_12_rules_are_curated_and_diverge_by_dialect() {
        // A curated title, not the humanized `derive` fallback ("Case to coalesce").
        let c = enrich(&finding("case-to-coalesce", None), Dialect::Postgres);
        assert_ne!(c.title, "Case to coalesce");
        assert!(!c.remedies.is_empty() && c.remedies.iter().all(|r| r.example.is_some()));

        // `like-on-numeric-column` diverges: Postgres errors, MySQL scans.
        let pg = enrich(&finding("like-on-numeric-column", None), Dialect::Postgres);
        let my = enrich(&finding("like-on-numeric-column", None), Dialect::Mysql);
        assert!(pg.why.to_lowercase().contains("error"), "pg: {}", pg.why);
        assert!(my.why.to_lowercase().contains("scan"), "my: {}", my.why);

        // `select-non-grouped-column`: rejected on Postgres, nondeterministic on SQLite.
        let pg2 = enrich(
            &finding("select-non-grouped-column", None),
            Dialect::Postgres,
        );
        let sq = enrich(&finding("select-non-grouped-column", None), Dialect::Sqlite);
        assert_ne!(pg2.why, sq.why);
        assert!(
            sq.why.to_lowercase().contains("nondeterministic"),
            "sqlite: {}",
            sq.why
        );
    }

    #[test]
    fn v0_3_12_2_rules_curated_and_order_by_diverges() {
        // Curated title, not the humanized fallback ("Sum case to count filter").
        let c = enrich(
            &finding("sum-case-to-count-filter", None),
            Dialect::Postgres,
        );
        assert_ne!(c.title, "Sum case to count filter");
        assert!(!c.remedies.is_empty());

        // `order-by-not-in-distinct-select` diverges: Postgres rejects, MySQL orders arbitrarily.
        let pg = enrich(
            &finding("order-by-not-in-distinct-select", None),
            Dialect::Postgres,
        );
        let my = enrich(
            &finding("order-by-not-in-distinct-select", None),
            Dialect::Mysql,
        );
        assert!(pg.why.to_lowercase().contains("reject"), "pg: {}", pg.why);
        assert!(
            my.why.to_lowercase().contains("arbitrary"),
            "my: {}",
            my.why
        );
    }
}
