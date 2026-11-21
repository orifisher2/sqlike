//! The equivalence verdict — a per-property vector, not a scalar (`docs/04b`).
//!
//! Comparing two queries yields a yes/no verdict *per property* of their result tables.
//! "Equivalent" is just the case where every property matches; the two weak outcomes
//! ("differs", "undecided") still report which properties hold and which break, so a caller
//! can act on a partial result instead of a bare "unknown". Every decided facet also records
//! *how* it was decided — its [`Confidence`].

use serde::{Deserialize, Serialize};

/// How a facet verdict was reached, ordered by strength: `Empirical` (sampling — could miss a
/// counterexample) < `Bounded` (no counterexample under a bounded-model search — a symbolic
/// guarantee over *all* instances up to the bound, stronger than sampling but not a full proof) <
/// `Structural` (proof by normalized-tree comparison) < `Formal` (external machine-checked proof).
/// A verdict's summary confidence is the *min* over its decided facets, i.e. the weakest evidence
/// behind the aggregate claim. **Declaration order is the strength order** (`derive(Ord)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Empirical,
    Bounded,
    Structural,
    Formal,
}

/// A fact the input did not state, which a decided verdict rests on.
///
/// Typed rather than prose so a caller can check it against their own database instead of reading
/// it once and forgetting it. An assumption is to a *premise* what [`Confidence`] is to *strength*:
/// both qualify a decided verdict without changing what it decided, which is why neither appears in
/// [`Overall`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assumption {
    /// No column carries a `COLLATE` that overrides the dialect's default, so text compares the way
    /// that dialect compares it out of the box — exactly on Postgres, SQLite and DuckDB,
    /// case-insensitively on MySQL, MariaDB and SQL Server. The schema model does not record
    /// `COLLATE`, so this cannot be checked, only stated.
    DefaultCollation,
    /// A cast of an integer value to an integer type was read as the value itself. That holds for
    /// every value the target type can carry; a value it cannot is an engine error, not a
    /// different row — but which values those are depends on widths the type model does not keep.
    NoOverflow,
    /// A text column holds ISO-8601 dates (`YYYY-MM-DD`, with an optional time part), so a
    /// `strftime` year or month read from it and a text range over it select the same rows.
    /// SQLite declares no date type and stores dates this way by convention; a value in another
    /// form breaks the premise, and the schema cannot say which form the column holds.
    IsoDateText,
}

/// The verdict for a single property of the result table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FacetVerdict {
    /// Proven equal, at this confidence. `bound` carries the search bound `K` when `by = Bounded`
    /// (equal on all instances up to `K`), and is `None` for every other confidence — so it's
    /// omitted from the JSON except on a bounded match.
    Match {
        by: Confidence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bound: Option<u32>,
        /// A fact the input did not state that this match rests on; omitted when there is none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assuming: Option<Assumption>,
    },
    /// Proven different; `detail` is a human description of the difference (a rename
    /// `b -> beta`, `count(*)->0 vs sum(1)->NULL on empty input`, …).
    Differ {
        detail: String,
        by: Confidence,
        /// As on `Match`: a difference can rest on an unstated fact just as a match can.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assuming: Option<Assumption>,
    },
    /// Neither proven equal nor proven different — carrying a short `reason` (which tier or
    /// construct left it open), so an undecided facet is never a bare "unknown" (the `04b`
    /// transparent-reasoning principle). A whole-verdict decline (out-of-scope construct) sets
    /// every facet to `Undecided` with the same reason.
    Undecided { reason: String },
    /// The property doesn't apply here (e.g. `order` when neither query defines one).
    NotApplicable,
}

impl FacetVerdict {
    /// A plain match at `by` (no bound — every confidence except `Bounded`).
    pub fn matched(by: Confidence) -> Self {
        FacetVerdict::Match {
            by,
            bound: None,
            assuming: None,
        }
    }

    /// A bounded match: equal on all instances up to `k` rows per table (`by = Bounded`).
    pub fn bounded(k: u32) -> Self {
        FacetVerdict::Match {
            by: Confidence::Bounded,
            bound: Some(k),
            assuming: None,
        }
    }

    /// The same verdict, resting on a fact the input did not state.
    pub fn assuming(self, a: Assumption) -> Self {
        match self {
            FacetVerdict::Match { by, bound, .. } => FacetVerdict::Match {
                by,
                bound,
                assuming: Some(a),
            },
            FacetVerdict::Differ { detail, by, .. } => FacetVerdict::Differ {
                detail,
                by,
                assuming: Some(a),
            },
            // Nothing was decided, so there is no premise to record.
            other => other,
        }
    }

    /// The assumption this verdict rests on, if any.
    pub fn assumption(&self) -> Option<Assumption> {
        match self {
            FacetVerdict::Match { assuming, .. } | FacetVerdict::Differ { assuming, .. } => {
                *assuming
            }
            _ => None,
        }
    }

    fn is_differ(&self) -> bool {
        matches!(self, FacetVerdict::Differ { .. })
    }

    fn is_undecided(&self) -> bool {
        matches!(self, FacetVerdict::Undecided { .. })
    }

    /// The confidence of a *decided* facet (`Match`/`Differ`); `None` for undecided / N/A.
    fn confidence(&self) -> Option<Confidence> {
        match self {
            FacetVerdict::Match { by, .. } | FacetVerdict::Differ { by, .. } => Some(*by),
            FacetVerdict::Undecided { .. } | FacetVerdict::NotApplicable => None,
        }
    }
}

/// The four facets of the output column schema, split out because they carry different weight:
/// `arity`/`types` are **data-affecting** (a difference means different results); `names` is a
/// note (a column's label is not part of the result); `position` names the columns that moved,
/// and since a result's columns are ordered, a move is never equivalent, though it is not a proven
/// difference either: the moved columns may hold the same values. See [`Overall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnFacets {
    pub arity: FacetVerdict,
    pub names: FacetVerdict,
    pub types: FacetVerdict,
    pub position: FacetVerdict,
}

/// The full facet vector for a comparison. Row comparison aligns columns **by name**, so
/// `columns.position` carries the order: `rows` and `position` together are the result the SQL
/// standard defines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyReport {
    pub columns: ColumnFacets,
    /// The row **multiset** (bag: duplicates count), aligned by column name.
    pub rows: FacetVerdict,
    /// Row count — strictly weaker than `rows` (equal rows ⇒ equal count), but sometimes
    /// decidable when `rows` is not.
    pub cardinality: FacetVerdict,
    /// The row **sequence**, when an ORDER BY defines one. Contract-affecting *unless* a
    /// LIMIT/OFFSET couples it into `rows` (then the row-set itself depends on the order).
    pub order: FacetVerdict,
}

/// How a result's columns are read (W1b). By position is the standard's: the columns in order,
/// their names labels. By name is for code that fetches columns by name. The two differ only when
/// the queries use the same output names in another order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnReading {
    #[default]
    ByPosition,
    ByName,
}

/// The scalar summary, derived from the facet vector — a convenience, never the whole answer
/// (the vector is). Serialized snake_case: `equivalent`, `equivalent_with_notes`, `differs`,
/// `undecided`. The CLI exit-code / `--fail-on` mapping lives in the surface layer (E2), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Overall {
    /// Every facet matches (or doesn't apply).
    Equivalent,
    /// Every data-affecting facet matches in the reading asked; only a renamed or moved column or
    /// a changed ordering guarantee differs or is unknown. The high-value middle a
    /// scalar verdict erases.
    EquivalentWithNotes,
    /// At least one data-affecting facet (arity, types, rows, cardinality) is proven different.
    Differs,
    /// No data-affecting facet is proven different, but at least one couldn't be decided — so
    /// equivalence can't be claimed. Never reads as equivalent (the one overclaim the design forbids).
    Undecided,
    /// Neither query runs, so there is nothing to compare. Not a coverage limit (that is
    /// `Undecided`) and not a difference: a statement about the inputs, carried by
    /// [`EquivalenceVerdict::reasons`]. The only `Overall` not derived from the facet vector.
    NotComparable,
}

// Hand-written so an unrecognized string decodes as `Undecided` rather than failing. A client
// pinned before a new variant existed then degrades to "couldn't tell" instead of erroring on the
// whole response — the one overclaim the design forbids is reading as equivalent, and this never
// does that. Every future outcome is safe for old clients because of this.
impl<'de> Deserialize<'de> for Overall {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "equivalent" => Overall::Equivalent,
            "equivalent_with_notes" => Overall::EquivalentWithNotes,
            "differs" => Overall::Differs,
            "not_comparable" => Overall::NotComparable,
            _ => Overall::Undecided,
        })
    }
}

/// Why one query cannot run, for [`Overall::NotComparable`]. Plain sentences, already
/// carrier-safe: they name the query and the offending reference, never a bare status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unrunnable {
    /// Why the first query does not run.
    pub a: String,
    /// Why the second query does not run.
    pub b: String,
}

/// The result of comparing two queries: the facet vector, its derived scalar summary, and the
/// summary confidence. Serializes to the `/v1/equivalence` response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquivalenceVerdict {
    pub overall: Overall,
    /// The **min** confidence over decided facets (weakest evidence) — `None` when nothing
    /// was decided (a fully-undecided verdict).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    /// Every fact the decided facets rest on that the input did not state — the union over the
    /// vector, so a caller reading only the scalar still sees that a premise is in play. Empty for
    /// an unconditional verdict, and omitted from the JSON then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<Assumption>,
    /// Set only for [`Overall::NotComparable`]: why each query could not run. `None` otherwise,
    /// and omitted from the JSON then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasons: Option<Unrunnable>,
    pub facets: PropertyReport,
}

impl EquivalenceVerdict {
    /// Build a verdict from a facet vector, deriving `overall` and the summary `confidence`
    /// per the `docs/04b` decision table. This is the type's invariant: the derived fields
    /// are always consistent with the facets, so a verdict can't be constructed inconsistently.
    /// The one exception is [`Self::not_comparable`], a statement about the inputs rather than the
    /// outputs, which no facet vector could produce.
    pub fn from_facets(facets: PropertyReport) -> Self {
        let overall = derive_overall(&facets);
        let confidence = summary_confidence(&facets);
        let assumptions = collect_assumptions(&facets);
        Self {
            overall,
            confidence,
            assumptions,
            reasons: None,
            facets,
        }
    }

    /// The verdict for a caller that does not care in what order the rows arrive (`--ignore order`,
    /// OR1b): two orders that can differ are a note, as they were before the standard made row
    /// order part of the result. Exact, since `rows` does not depend on the order unless a cut
    /// does, and under a cut it is already `Undecided` when the orders differ. A verdict with
    /// `reasons` (neither query runs) is returned as it is.
    pub fn ignoring_order(self) -> Self {
        if self.reasons.is_some() {
            return self;
        }
        let mut facets = self.facets;
        if let FacetVerdict::Undecided { reason } = &facets.order {
            facets.order = FacetVerdict::Differ {
                detail: reason.clone(),
                by: Confidence::Structural,
                assuming: None,
            };
        }
        Self::from_facets(facets)
    }

    /// The verdict for a pair where neither query runs. The one verdict not derived from a facet
    /// vector: it is a statement about the inputs, so the facets are all `Undecided` (there is no
    /// result to compare) and the answer lives in `reasons`.
    pub fn not_comparable(a: String, b: String) -> Self {
        let u = || FacetVerdict::Undecided {
            reason: "neither query runs".to_string(),
        };
        Self {
            overall: Overall::NotComparable,
            confidence: None,
            assumptions: Vec::new(),
            reasons: Some(Unrunnable { a, b }),
            facets: PropertyReport {
                columns: ColumnFacets {
                    arity: u(),
                    names: u(),
                    types: u(),
                    position: u(),
                },
                rows: u(),
                cardinality: u(),
                order: u(),
            },
        }
    }

    /// Serialize the verdict to its JSON envelope (the shape front doors forward).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("verdict serializes")
    }
}

/// The distinct assumptions the decided facets rest on, in a stable order.
fn collect_assumptions(r: &PropertyReport) -> Vec<Assumption> {
    let mut out = Vec::new();
    for f in all_facets(r) {
        if let Some(a) = f.assumption() {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out
}

/// Facets whose difference means genuinely different results.
fn data_facets(r: &PropertyReport) -> [&FacetVerdict; 4] {
    [&r.columns.arity, &r.columns.types, &r.rows, &r.cardinality]
}

/// Facets whose difference is a presentation/contract note, not a data change.
fn contract_facets(r: &PropertyReport) -> [&FacetVerdict; 3] {
    [&r.columns.names, &r.columns.position, &r.order]
}

fn all_facets(r: &PropertyReport) -> [&FacetVerdict; 7] {
    let [a, b, c, d] = data_facets(r);
    let [e, f, g] = contract_facets(r);
    [a, b, c, d, e, f, g]
}

/// The `docs/04b` fold. Precedence: a data-affecting difference dominates; an *undecided*
/// data facet means we can't confirm the data is equal (→ Undecided), and so does a column order
/// that is unknown, since the reading `rows` was decided in may not be the one asked (W1b); the
/// contract facets, a moved column included, only ever soften a data-clean verdict to a note.
/// (Reading of 04b's ambiguous Undecided clause: "at least one couldn't be decided" means a *data*
/// facet — an undecided *contract* facet leaves the proven-equal data intact, so it's a note.)
fn derive_overall(r: &PropertyReport) -> Overall {
    let data = data_facets(r);
    if data.iter().any(|f| f.is_differ()) {
        Overall::Differs
    } else if data.iter().any(|f| f.is_undecided())
        || r.columns.position.is_undecided()
        || r.order.is_undecided()
    {
        Overall::Undecided
    } else if contract_facets(r)
        .iter()
        .any(|f| f.is_differ() || f.is_undecided())
    {
        Overall::EquivalentWithNotes
    } else {
        Overall::Equivalent
    }
}

/// The weakest evidence behind any decided facet, or `None` if nothing was decided.
fn summary_confidence(r: &PropertyReport) -> Option<Confidence> {
    all_facets(r)
        .into_iter()
        .filter_map(|f| f.confidence())
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(by: Confidence) -> FacetVerdict {
        FacetVerdict::matched(by)
    }

    fn differ(by: Confidence) -> FacetVerdict {
        FacetVerdict::Differ {
            assuming: None,
            detail: "x".into(),
            by,
        }
    }

    fn undecided() -> FacetVerdict {
        FacetVerdict::Undecided { reason: "x".into() }
    }

    fn all_match(by: Confidence) -> PropertyReport {
        PropertyReport {
            columns: ColumnFacets {
                arity: m(by),
                names: m(by),
                types: m(by),
                position: m(by),
            },
            rows: m(by),
            cardinality: m(by),
            order: m(by),
        }
    }

    /// OR1b: two orders that can differ cap `overall`, like a moved column; a caller that does not
    /// care about row order reads them as a note.
    #[test]
    fn an_undecided_order_caps_overall_and_folds_to_a_note() {
        let mut r = all_match(Confidence::Structural);
        r.order = undecided();
        let v = EquivalenceVerdict::from_facets(r);
        assert_eq!(v.overall, Overall::Undecided);
        assert_eq!(v.ignoring_order().overall, Overall::EquivalentWithNotes);

        let mut r = all_match(Confidence::Structural);
        r.rows = undecided();
        r.order = undecided();
        let v = EquivalenceVerdict::from_facets(r).ignoring_order();
        assert_eq!(v.overall, Overall::Undecided, "the rows are still unknown");

        let v = EquivalenceVerdict::from_facets(all_match(Confidence::Structural));
        assert_eq!(v.clone().ignoring_order(), v, "nothing to fold");
    }

    #[test]
    fn all_match_is_equivalent() {
        let v = EquivalenceVerdict::from_facets(all_match(Confidence::Structural));
        assert_eq!(v.overall, Overall::Equivalent);
        assert_eq!(v.confidence, Some(Confidence::Structural));
    }

    #[test]
    fn name_difference_is_note_not_differs() {
        let mut r = all_match(Confidence::Structural);
        r.columns.names = differ(Confidence::Structural);
        assert_eq!(
            EquivalenceVerdict::from_facets(r).overall,
            Overall::EquivalentWithNotes
        );
    }

    #[test]
    fn row_difference_is_differs() {
        let mut r = all_match(Confidence::Empirical);
        r.rows = differ(Confidence::Empirical);
        assert_eq!(EquivalenceVerdict::from_facets(r).overall, Overall::Differs);
    }

    #[test]
    fn undecided_rows_is_undecided_never_equivalent() {
        let mut r = all_match(Confidence::Structural);
        r.rows = undecided();
        assert_eq!(
            EquivalenceVerdict::from_facets(r).overall,
            Overall::Undecided
        );
    }

    #[test]
    fn order_difference_alone_is_note() {
        let mut r = all_match(Confidence::Structural);
        r.order = differ(Confidence::Structural);
        assert_eq!(
            EquivalenceVerdict::from_facets(r).overall,
            Overall::EquivalentWithNotes
        );
    }

    #[test]
    fn data_differ_outranks_contract_note() {
        let mut r = all_match(Confidence::Structural);
        r.columns.names = differ(Confidence::Structural); // contract note
        r.rows = differ(Confidence::Structural); // data difference
        assert_eq!(EquivalenceVerdict::from_facets(r).overall, Overall::Differs);
    }

    #[test]
    fn undecided_contract_facet_softens_to_note() {
        let mut r = all_match(Confidence::Structural);
        r.columns.names = undecided(); // data proven equal, labels unknown
        let v = EquivalenceVerdict::from_facets(r);
        assert_eq!(v.overall, Overall::EquivalentWithNotes);
    }

    // `rows` is decided in the reading asked for (W1b), so a moved column is the layout note it
    // names: whether the values moved with it is `rows`'s question.
    #[test]
    fn a_moved_column_is_a_note() {
        let mut r = all_match(Confidence::Structural);
        r.columns.position = differ(Confidence::Structural);
        assert_eq!(
            EquivalenceVerdict::from_facets(r).overall,
            Overall::EquivalentWithNotes
        );
    }

    #[test]
    fn an_unknown_column_order_is_never_equivalent() {
        let mut r = all_match(Confidence::Structural);
        r.columns.position = undecided();
        assert_eq!(
            EquivalenceVerdict::from_facets(r).overall,
            Overall::Undecided
        );
    }

    #[test]
    fn a_moved_column_over_different_rows_differs() {
        let mut r = all_match(Confidence::Structural);
        r.columns.position = differ(Confidence::Structural);
        r.rows = differ(Confidence::Structural);
        assert_eq!(EquivalenceVerdict::from_facets(r).overall, Overall::Differs);
    }

    #[test]
    fn summary_confidence_is_weakest_decided() {
        let mut r = all_match(Confidence::Structural);
        r.rows = m(Confidence::Empirical); // weaker evidence on one facet
        assert_eq!(
            EquivalenceVerdict::from_facets(r).confidence,
            Some(Confidence::Empirical)
        );
    }

    #[test]
    fn summary_confidence_ignores_undecided_and_not_applicable() {
        let mut r = all_match(Confidence::Formal);
        r.order = FacetVerdict::NotApplicable;
        r.cardinality = undecided(); // data undecided → overall Undecided...
        let v = EquivalenceVerdict::from_facets(r);
        assert_eq!(v.overall, Overall::Undecided);
        assert_eq!(v.confidence, Some(Confidence::Formal)); // ...but the decided facets are Formal
    }

    #[test]
    fn serde_round_trips() {
        let v = EquivalenceVerdict::from_facets(all_match(Confidence::Formal));
        let json = serde_json::to_string(&v).unwrap();
        let back: EquivalenceVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn bounded_sits_between_empirical_and_structural() {
        assert!(Confidence::Empirical < Confidence::Bounded);
        assert!(Confidence::Bounded < Confidence::Structural);
        assert!(Confidence::Structural < Confidence::Formal);
    }

    #[test]
    fn bounded_rows_with_a_contract_note_summarizes_bounded() {
        // rows proven equal by the bounded verifier; a rename is a Structural contract note.
        let mut r = all_match(Confidence::Structural);
        r.rows = FacetVerdict::bounded(3);
        r.columns.names = differ(Confidence::Structural);
        let v = EquivalenceVerdict::from_facets(r);
        assert_eq!(v.overall, Overall::EquivalentWithNotes); // Overall unchanged — no new variant
        assert_eq!(v.confidence, Some(Confidence::Bounded)); // weakest decided facet
    }

    #[test]
    fn bounded_match_serializes_its_bound_but_a_plain_match_omits_it() {
        let bounded = serde_json::to_string(&FacetVerdict::bounded(3)).unwrap();
        assert!(bounded.contains("\"bound\":3"), "{bounded}");
        let plain = serde_json::to_string(&FacetVerdict::matched(Confidence::Structural)).unwrap();
        assert!(!plain.contains("bound"), "{plain}");
        // round-trips back to the same value
        let back: FacetVerdict = serde_json::from_str(&bounded).unwrap();
        assert_eq!(back, FacetVerdict::bounded(3));
    }

    #[test]
    fn wire_format_is_snake_case_tagged() {
        let mut r = all_match(Confidence::Structural);
        r.columns.names = differ(Confidence::Structural);
        let val = serde_json::to_value(EquivalenceVerdict::from_facets(r)).unwrap();
        assert_eq!(val["overall"], "equivalent_with_notes");
        assert_eq!(val["confidence"], "structural");
        assert_eq!(val["facets"]["columns"]["names"]["verdict"], "differ");
        assert_eq!(val["facets"]["rows"]["verdict"], "match");
        assert_eq!(val["facets"]["rows"]["by"], "structural");
    }

    #[test]
    fn not_comparable_carries_both_reasons_and_serializes() {
        let v = EquivalenceVerdict::not_comparable("A broke".into(), "B broke".into());
        assert_eq!(v.overall, Overall::NotComparable);
        let val: serde_json::Value = serde_json::from_str(&v.to_json()).unwrap();
        assert_eq!(val["overall"], "not_comparable");
        assert_eq!(val["reasons"]["a"], "A broke");
        assert_eq!(val["reasons"]["b"], "B broke");
    }

    /// A client built before `not_comparable` existed must not fail to decode it: the hand-written
    /// `Deserialize` maps any unknown `overall` to `Undecided`, so an old pinned client degrades to
    /// "couldn't tell" rather than erroring on the whole response. Simulated with a future variant.
    #[test]
    fn an_unknown_overall_decodes_as_undecided() {
        let body = r#"{"overall":"some_future_verdict","facets":{
            "columns":{"arity":{"verdict":"match","by":"structural"},
                "names":{"verdict":"match","by":"structural"},
                "types":{"verdict":"match","by":"structural"},
                "position":{"verdict":"match","by":"structural"}},
            "rows":{"verdict":"match","by":"structural"},
            "cardinality":{"verdict":"match","by":"structural"},
            "order":{"verdict":"not_applicable"}}}"#;
        let v: EquivalenceVerdict = serde_json::from_str(body).unwrap();
        assert_eq!(v.overall, Overall::Undecided);
    }
}
