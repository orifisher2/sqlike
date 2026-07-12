//! The equivalence verdict — a per-property vector, not a scalar (`docs/04b`).
//!
//! Comparing two queries yields a yes/no verdict *per property* of their result tables.
//! "Equivalent" is just the case where every property matches; the two weak outcomes
//! ("differs", "undecided") still report which properties hold and which break, so a caller
//! can act on a partial result instead of a bare "unknown". Every decided facet also records
//! *how* it was decided — its [`Confidence`].

use serde::{Deserialize, Serialize};

/// How a facet verdict was reached, ordered by strength: `Empirical` (sampling — could miss a
/// counterexample) < `Structural` (proof by normalized-tree comparison) < `Formal` (external
/// machine-checked proof). A verdict's summary confidence is the *min* over its decided facets,
/// i.e. the weakest evidence behind the aggregate claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Empirical,
    Structural,
    Formal,
}

/// The verdict for a single property of the result table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum FacetVerdict {
    /// Proven equal, at this confidence.
    Match { by: Confidence },
    /// Proven different; `detail` is a human description of the difference (a rename
    /// `b -> beta`, `count(*)->0 vs sum(1)->NULL on empty input`, …).
    Differ { detail: String, by: Confidence },
    /// Neither proven equal nor proven different — carrying a short `reason` (which tier or
    /// construct left it open), so an undecided facet is never a bare "unknown" (the `04b`
    /// transparent-reasoning principle). A whole-verdict decline (out-of-scope construct) sets
    /// every facet to `Undecided` with the same reason.
    Undecided { reason: String },
    /// The property doesn't apply here (e.g. `order` when neither query defines one).
    NotApplicable,
}

impl FacetVerdict {
    fn is_differ(&self) -> bool {
        matches!(self, FacetVerdict::Differ { .. })
    }

    fn is_undecided(&self) -> bool {
        matches!(self, FacetVerdict::Undecided { .. })
    }

    /// The confidence of a *decided* facet (`Match`/`Differ`); `None` for undecided / N/A.
    fn confidence(&self) -> Option<Confidence> {
        match self {
            FacetVerdict::Match { by } | FacetVerdict::Differ { by, .. } => Some(*by),
            FacetVerdict::Undecided { .. } | FacetVerdict::NotApplicable => None,
        }
    }
}

/// The four facets of the output column schema, split out because they carry different weight:
/// `arity`/`types` are **data-affecting** (a difference means different results), while
/// `names`/`position` are **contract-affecting** (the values are identical; only how you
/// address them changed — callers fetch by name, so a rename or reorder is a note, not a
/// difference). See [`Overall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnFacets {
    pub arity: FacetVerdict,
    pub names: FacetVerdict,
    pub types: FacetVerdict,
    pub position: FacetVerdict,
}

/// The full facet vector for a comparison. Row comparison aligns columns **by name** (callers
/// fetch by name), so `columns.position` never affects `rows` — it is a pure presentation note.
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

/// The scalar summary, derived from the facet vector — a convenience, never the whole answer
/// (the vector is). Serialized snake_case: `equivalent`, `equivalent_with_notes`, `differs`,
/// `undecided`. The CLI exit-code / `--fail-on` mapping lives in the surface layer (E2), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overall {
    /// Every facet matches (or doesn't apply).
    Equivalent,
    /// Every data-affecting facet matches; only contract-affecting facets differ or are
    /// unknown (a renamed/reordered column, a changed ordering guarantee). The high-value
    /// middle a scalar verdict erases.
    EquivalentWithNotes,
    /// At least one data-affecting facet (arity, types, rows, cardinality) is proven different.
    Differs,
    /// No data-affecting facet is proven different, but at least one couldn't be decided — so
    /// equivalence can't be claimed. Never reads as equivalent (the one overclaim the design forbids).
    Undecided,
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
    pub facets: PropertyReport,
}

impl EquivalenceVerdict {
    /// Build a verdict from a facet vector, deriving `overall` and the summary `confidence`
    /// per the `docs/04b` decision table. This is the type's invariant: the two derived fields
    /// are always consistent with the facets, so a verdict can't be constructed inconsistently.
    pub fn from_facets(facets: PropertyReport) -> Self {
        let overall = derive_overall(&facets);
        let confidence = summary_confidence(&facets);
        Self {
            overall,
            confidence,
            facets,
        }
    }

    /// Serialize the verdict to its JSON envelope (the shape front doors forward).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("verdict serializes")
    }
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
/// data facet means we can't confirm the data is equal (→ Undecided); contract facets only
/// ever soften a data-clean verdict to a note. (Reading of 04b's ambiguous Undecided clause:
/// "at least one couldn't be decided" means a *data* facet — an undecided *contract* facet
/// leaves the proven-equal data intact, so it's a note, not Undecided.)
fn derive_overall(r: &PropertyReport) -> Overall {
    let data = data_facets(r);
    if data.iter().any(|f| f.is_differ()) {
        Overall::Differs
    } else if data.iter().any(|f| f.is_undecided()) {
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
        FacetVerdict::Match { by }
    }

    fn differ(by: Confidence) -> FacetVerdict {
        FacetVerdict::Differ {
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
        r.columns.position = undecided(); // data proven equal, presentation unknown
        let v = EquivalenceVerdict::from_facets(r);
        assert_eq!(v.overall, Overall::EquivalentWithNotes);
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
}
