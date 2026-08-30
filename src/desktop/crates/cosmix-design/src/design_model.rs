use std::collections::BTreeMap;
use std::time::SystemTime;

use crate::{
    ButtonCellKey, ButtonProperty, ButtonTypographyKey, DerivationRecipe, DesignDiagnostic,
    FocusRingProvenance, PairRefDecision, PairRefExclusion, ResolvedButtonTable, ResolvedColours,
    ResolvedPair, ResolvedTypeRecord,
};

/// Stable identity for diagnostics and source mutation surfaces.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceIdentity(String);

impl SourceIdentity {
    pub fn new(identity: impl Into<String>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A monotonically increasing applied-design revision.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DesignRevision(u64);

impl DesignRevision {
    pub const FIRST: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next consecutive revision, or `None` instead of wrapping.
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The revision an artifact replacing `current` receives.
    ///
    /// Split out from the apply path so the exhaustion arm is reachable in a
    /// test without constructing an artifact at `u64::MAX`.
    pub const fn succeeding(current: Option<Self>) -> Option<Self> {
        match current {
            None => Some(Self::FIRST),
            Some(revision) => revision.next(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTables {
    pub button: ResolvedButtonTable,
}

/// How an admitted pair override is resolved for one compiled cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PairOverrideRoute<'a> {
    ReplaceWhole {
        pair: &'a ResolvedPair,
        ring_recipe: Option<&'a DerivationRecipe>,
    },
    Reexecute {
        pair: &'a ResolvedPair,
        recipe: &'a DerivationRecipe,
        ring_recipe: Option<&'a DerivationRecipe>,
    },
}

/// The artifact's complete answer for one pair override query.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PairOverrideDisposition<'a> {
    UnknownPair,
    Available(PairOverrideRoute<'a>),
    Unavailable(&'a PairRefExclusion),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedDictionary {
    pub colours: ResolvedColours,
    pub metrics: BTreeMap<String, ResolvedMetric>,
    pub scales: BTreeMap<String, Vec<f64>>,
}

/// Metric kinds that survive compilation. An authored step is resolved to px,
/// while a ratio remains dimensionless.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvedMetricKind {
    Px,
    Ratio,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedMetric {
    pub kind: ResolvedMetricKind,
    pub value: f64,
}

/// The resolved type scale plus the per-family assignment tables.
///
/// Fields are private because the two members are not independent: every
/// assignment names a record that must exist in the scale. The checked
/// constructor is the only way to build one, so a resolver can look an
/// assignment up without handling a missing record.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTypography {
    scale: BTreeMap<String, ResolvedTypeRecord>,
    button: crate::ButtonTypographyTable,
}

/// A typography assignment resolved against the scale in one hop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedTypographyRef<'a> {
    pub name: &'a str,
    pub record: &'a ResolvedTypeRecord,
}

impl ResolvedTypography {
    /// Builds the resolved typography, checking that every assignment resolves.
    ///
    /// # Panics
    /// Panics if any assignment names a record absent from the scale. The
    /// compiler validates typography references before it reaches here, so a
    /// panic is a compiler bug rather than a bad-source outcome.
    #[cfg(feature = "compiler")]
    pub(crate) fn new(
        scale: BTreeMap<String, ResolvedTypeRecord>,
        button: crate::ButtonTypographyTable,
    ) -> Self {
        for variant in crate::ButtonVariant::ALL {
            for size in crate::ButtonSize::ALL {
                for part in crate::ButtonPart::ALL {
                    let key = ButtonTypographyKey {
                        variant,
                        size,
                        part,
                    };
                    let name = &button.assignment(key).record_name;
                    assert!(
                        scale.contains_key(name),
                        "button typography assignment {key:?} names `{name}`, absent from the scale"
                    );
                }
            }
        }
        Self { scale, button }
    }

    pub fn scale(&self) -> &BTreeMap<String, ResolvedTypeRecord> {
        &self.scale
    }

    pub fn record(&self, name: &str) -> Option<&ResolvedTypeRecord> {
        self.scale.get(name)
    }

    /// Resolves a button coordinate to its named record. Total by construction.
    pub fn button(&self, key: ButtonTypographyKey) -> ResolvedTypographyRef<'_> {
        let name = &self.button.assignment(key).record_name;
        let record = self
            .scale
            .get(name)
            .expect("checked at construction: every assignment resolves");
        ResolvedTypographyRef { name, record }
    }

    pub fn button_assignments(&self) -> &crate::ButtonTypographyTable {
        &self.button
    }
}

/// Stable IDs for every resolved dictionary, assignment, and table value.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DesignValueId {
    ColourPrimitive(String),
    ColourPair(String),
    NonTextColour(String),
    Metric(String),
    ScaleEntry {
        scale: String,
        index: usize,
    },
    TypeRecord(String),
    ButtonCell {
        key: ButtonCellKey,
        property: ButtonProperty,
    },
    ButtonTypography(ButtonTypographyKey),
}

/// The metric quantity as it appeared in source, before step resolution.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthoredMetric {
    Px { value: f64 },
    Step { scale: String, index: usize },
    Ratio { value: f64 },
}

/// One-hop trace information for a resolved value.
///
/// For a dictionary value, `token_path` lists the dictionary dependencies used
/// to produce it, `applied_rule` is the authored rule or compiler generator
/// that defines the record, and `value_origin_rule` is the immediate rule or
/// generator credited with its resolved value. A semantic pair can therefore
/// carry parallel surface, foreground, and backdrop dependencies; a step metric
/// names its scale entry while crediting that entry's rule or generator.
///
/// For a mapping cell, `token_path` lists the dictionary or typography values
/// selected by the mapping. A derived pair can list both its pair and ratio
/// inputs. `applied_rule` is the winning mapping rule, while
/// `value_origin_rule` is the mapping rule that supplied the selected value.
/// They normally match. For an explicit reset, `applied_rule` is the reset and
/// `value_origin_rule` is the base mapping rule whose value was restored.
///
/// Across both record shapes, token paths are dependencies rather than rule
/// ownership. No invariant ties `value_origin_rule` to a `token_path` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ValueProvenance {
    /// Named dictionary or typography dependencies used by this value.
    pub token_path: Vec<String>,
    /// The rule or generator that made the decision at this record's layer.
    pub applied_rule: String,
    /// The rule or generator credited with the selected value after resolution.
    pub value_origin_rule: String,
    /// The source quantity before step-to-px resolution, when this value is or
    /// directly consumes a metric.
    pub authored_metric: Option<AuthoredMetric>,
    /// Focus-ring walk trace when this value is a derived indicator.
    pub focus_ring: Option<FocusRingProvenance>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DesignProvenance {
    values: BTreeMap<DesignValueId, ValueProvenance>,
}

impl DesignProvenance {
    pub fn value(&self, id: &DesignValueId) -> Option<&ValueProvenance> {
        self.values.get(id)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[cfg(feature = "compiler")]
    pub(crate) fn insert(&mut self, id: DesignValueId, value: ValueProvenance) {
        self.values.insert(id, value);
    }

    #[cfg(feature = "compiler")]
    pub(crate) fn extend(&mut self, other: Self) {
        self.values.extend(other.values);
    }
}

/// A successfully compiled design before an apply assigns its revision.
#[derive(Clone, Debug, PartialEq)]
pub struct UnstampedResolvedDesign {
    source: SourceIdentity,
    tables: ResolvedTables,
    dictionary: ResolvedDictionary,
    typography: ResolvedTypography,
    provenance: DesignProvenance,
}

impl UnstampedResolvedDesign {
    #[cfg(feature = "compiler")]
    pub(crate) fn new(
        source: SourceIdentity,
        tables: ResolvedTables,
        dictionary: ResolvedDictionary,
        typography: ResolvedTypography,
        provenance: DesignProvenance,
    ) -> Self {
        Self {
            source,
            tables,
            dictionary,
            typography,
            provenance,
        }
    }

    pub fn source(&self) -> &SourceIdentity {
        &self.source
    }

    pub fn tables(&self) -> &ResolvedTables {
        &self.tables
    }

    pub fn dictionary(&self) -> &ResolvedDictionary {
        &self.dictionary
    }

    pub fn typography(&self) -> &ResolvedTypography {
        &self.typography
    }

    pub fn provenance(&self) -> &DesignProvenance {
        &self.provenance
    }

    pub fn button_pair_override(
        &self,
        key: ButtonCellKey,
        pair_name: &str,
    ) -> PairOverrideDisposition<'_> {
        button_pair_override(&self.tables, &self.dictionary, key, pair_name)
    }
}

/// The live design artifact.
///
/// **Deliberately not `Clone`, and deliberately opaque.** The apply path takes
/// the live artifact by value, and that linearity only guarantees anything
/// while the artifact cannot be duplicated. Dropping `Clone` alone is not
/// enough: with public fields, a holder of `&ResolvedDesign` could rebuild an
/// equal artifact field by field — every member is itself `Clone` — keep the
/// original, and apply both, producing two different artifacts that each claim
/// revision n+1. Private fields make [`apply_compiled_design`] the only way to
/// obtain one, so a lineage cannot fork. Publishing a field, or re-deriving
/// `Clone`, silently reopens that.
#[derive(Debug, PartialEq)]
pub struct ResolvedDesign {
    revision: DesignRevision,
    source: SourceIdentity,
    tables: ResolvedTables,
    dictionary: ResolvedDictionary,
    typography: ResolvedTypography,
    provenance: DesignProvenance,
    // Recipes deliberately have no top-level member: per-cell recipes are
    // authoritative, and another map would duplicate them and risk divergence.
}

impl ResolvedDesign {
    pub fn revision(&self) -> DesignRevision {
        self.revision
    }

    pub fn source(&self) -> &SourceIdentity {
        &self.source
    }

    pub fn tables(&self) -> &ResolvedTables {
        &self.tables
    }

    pub fn dictionary(&self) -> &ResolvedDictionary {
        &self.dictionary
    }

    pub fn typography(&self) -> &ResolvedTypography {
        &self.typography
    }

    pub fn provenance(&self) -> &DesignProvenance {
        &self.provenance
    }

    pub fn button_pair_override(
        &self,
        key: ButtonCellKey,
        pair_name: &str,
    ) -> PairOverrideDisposition<'_> {
        button_pair_override(&self.tables, &self.dictionary, key, pair_name)
    }
}

pub(crate) fn button_pair_override<'a>(
    tables: &'a ResolvedTables,
    dictionary: &'a ResolvedDictionary,
    key: ButtonCellKey,
    pair_name: &str,
) -> PairOverrideDisposition<'a> {
    let Some(pair) = dictionary.colours.pairs.get(pair_name) else {
        return PairOverrideDisposition::UnknownPair;
    };
    let cell = tables.button.cell(key);
    let mut ring_recipe = None;
    if let Some(recipe) = cell.ring_recipe.as_ref() {
        if let Some(policy) = recipe.substitution_policy() {
            assert_eq!(
                recipe.substitutable_slot,
                Some(policy.slot()),
                "a retained ring recipe's slot must match its compiled pair policy"
            );
            if let PairRefDecision::Excluded(reason) = policy
                .decision(pair_name)
                .expect("a compiled ring policy must classify every dictionary pair")
            {
                return PairOverrideDisposition::Unavailable(reason);
            }
            ring_recipe = Some(recipe);
        } else {
            // SPEC 19 §10.2: no slot means no re-execution. The pair override
            // remains available as a whole-value replacement, but the route
            // must not advertise this fixed ring recipe as repointable.
            debug_assert!(recipe.substitutable_slot.is_none());
        }
    }
    // A plain mapping reference clones its dictionary pair exactly, including
    // any palette-level provenance recipe. Such a recipe did not originate at
    // the cell and must not be re-executed by a cell override. A cell-owned
    // substitutable pair recipe cannot compare equal to its source dictionary
    // pair because evaluator movement is enforced; a fixed cell recipe takes
    // ReplaceWhole below either way.
    if dictionary.colours.pairs.get(&cell.pair_name) == Some(&cell.pair) {
        return PairOverrideDisposition::Available(PairOverrideRoute::ReplaceWhole {
            pair,
            ring_recipe,
        });
    }
    let Some(recipe) = cell.pair_recipe.as_ref() else {
        return PairOverrideDisposition::Available(PairOverrideRoute::ReplaceWhole {
            pair,
            ring_recipe,
        });
    };
    let Some(policy) = recipe.substitution_policy() else {
        assert!(
            recipe.substitutable_slot.is_none(),
            "a retained substitutable recipe must carry its compiled pair policy"
        );
        return PairOverrideDisposition::Available(PairOverrideRoute::ReplaceWhole {
            pair,
            ring_recipe,
        });
    };
    assert_eq!(
        recipe.substitutable_slot,
        Some(policy.slot()),
        "a retained recipe's slot must match its compiled pair policy"
    );
    match policy
        .decision(pair_name)
        .expect("a compiled pair policy must classify every dictionary pair")
    {
        PairRefDecision::Admitted => {
            PairOverrideDisposition::Available(PairOverrideRoute::Reexecute {
                pair,
                recipe,
                ring_recipe,
            })
        }
        PairRefDecision::Excluded(reason) => PairOverrideDisposition::Unavailable(reason),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesignCompileSuccess {
    pub candidate: UnstampedResolvedDesign,
    pub diagnostics: Vec<DesignDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesignCompileFailure {
    pub attempted_source: SourceIdentity,
    pub diagnostics: Vec<DesignDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
// The public success arm owns the complete resolved artifact. Boxing it would
// change the caller-facing result shape solely to satisfy a size heuristic.
#[allow(clippy::large_enum_variant)]
pub enum DesignCompileResult {
    Success(DesignCompileSuccess),
    Fatal(DesignCompileFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesignCompileOutcome {
    Fatal,
    SucceededWithWarnings,
    Clean,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesignCompileStatus {
    pub attempted_source: SourceIdentity,
    pub outcome: DesignCompileOutcome,
    pub diagnostics: Vec<DesignDiagnostic>,
    pub compiled_at: SystemTime,
}

/// What an apply did to the live artifact. The artifact itself travels in
/// [`DesignApplyTransition::design`]; this reports which way it went.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesignApplyDecision {
    /// The candidate was stamped and became the live artifact.
    Replaced,
    /// The compile was fatal; the previous artifact is unchanged.
    KeptCurrent,
}

/// The outcome of one apply: the live artifact afterwards, plus what happened.
///
/// `design` is `None` only when no artifact was live and the compile failed —
/// the boot-failure state, in which no resolver may run.
#[derive(Debug, PartialEq)]
pub struct DesignApplyTransition {
    pub design: Option<ResolvedDesign>,
    pub decision: DesignApplyDecision,
    pub status: DesignCompileStatus,
}

/// Pure last-known-good transition. Instance-override provenance is added by
/// the later override work; this layer records the compiled chain only.
///
/// Takes the live artifact **by value** and hands back the artifact that is
/// live afterwards. Because [`ResolvedDesign`] is neither `Clone` nor
/// externally constructible, a caller cannot retain the previous artifact while
/// producing a second transition from it: two candidates compiled against the
/// same revision cannot both be applied and collapse to one revision number.
/// That covers duplication of the artifact, and nothing more — it does not by
/// itself serialise two threads each holding their own lineage, which is the
/// owning store's job.
///
/// What this function does *not* decide is which source boots first. Revision 1
/// goes to whichever artifact is applied against `None`; SPEC 19 §11.6's
/// "embedded default is revision 1" is an obligation on the bootstrap owner
/// that installs the first artifact, and is not enforced here.
///
/// # Panics
/// Panics if the revision space is exhausted, which needs `u64::MAX`
/// successful applies; see [`DesignRevision::succeeding`].
pub fn apply_compiled_design(
    current: Option<ResolvedDesign>,
    result: DesignCompileResult,
    compiled_at: SystemTime,
) -> DesignApplyTransition {
    match result {
        DesignCompileResult::Fatal(failure) => DesignApplyTransition {
            design: current,
            decision: DesignApplyDecision::KeptCurrent,
            status: DesignCompileStatus {
                attempted_source: failure.attempted_source,
                outcome: DesignCompileOutcome::Fatal,
                diagnostics: failure.diagnostics,
                compiled_at,
            },
        },
        DesignCompileResult::Success(success) => {
            // A success carrying an error diagnostic is a contradiction — a
            // compiler bug rather than a source outcome. Refuse it exactly as a
            // fatal compile is refused, so the last known good stays live.
            // Panicking in debug and installing it in release would make the
            // invariant depend on the build profile, i.e. hold everywhere
            // except where it matters.
            if success
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == crate::DiagnosticSeverity::Error)
            {
                return DesignApplyTransition {
                    design: current,
                    decision: DesignApplyDecision::KeptCurrent,
                    status: DesignCompileStatus {
                        attempted_source: success.candidate.source,
                        outcome: DesignCompileOutcome::Fatal,
                        diagnostics: success.diagnostics,
                        compiled_at,
                    },
                };
            }
            let revision = DesignRevision::succeeding(current.map(|artifact| artifact.revision))
                .expect("design revision space exhausted");
            let outcome = if success.diagnostics.is_empty() {
                DesignCompileOutcome::Clean
            } else {
                DesignCompileOutcome::SucceededWithWarnings
            };
            let candidate = success.candidate;
            let attempted_source = candidate.source.clone();
            DesignApplyTransition {
                design: Some(ResolvedDesign {
                    revision,
                    source: candidate.source,
                    tables: candidate.tables,
                    dictionary: candidate.dictionary,
                    typography: candidate.typography,
                    provenance: candidate.provenance,
                }),
                decision: DesignApplyDecision::Replaced,
                status: DesignCompileStatus {
                    attempted_source,
                    outcome,
                    diagnostics: success.diagnostics,
                    compiled_at,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque_pair(name: &str, surface: crate::LinearRgba) -> ResolvedPair {
        ResolvedPair {
            surface_name: format!("{name}.surface"),
            surface,
            foreground_name: "white".into(),
            foreground: crate::LinearRgba::WHITE,
            backdrop_name: None,
            backdrop: None,
            rendered_surface: surface,
            rendered_foreground: crate::LinearRgba::WHITE,
            contrast_ratio: crate::contrast_ratio(crate::LinearRgba::WHITE, surface),
            recipe: None,
        }
    }

    #[test]
    fn maximum_revision_refuses_to_wrap() {
        assert_eq!(DesignRevision(u64::MAX).next(), None);
    }

    #[test]
    fn the_first_applied_artifact_takes_the_first_revision() {
        assert_eq!(
            DesignRevision::succeeding(None),
            Some(DesignRevision::FIRST)
        );
    }

    #[test]
    fn each_apply_advances_the_revision_by_one() {
        assert_eq!(
            DesignRevision::succeeding(Some(DesignRevision(7))),
            Some(DesignRevision(8))
        );
    }

    // The arm `apply_compiled_design` turns into a panic. Exercised here
    // rather than through apply, which would need an artifact at u64::MAX.
    #[test]
    fn succeeding_the_maximum_revision_refuses_to_wrap() {
        assert_eq!(
            DesignRevision::succeeding(Some(DesignRevision(u64::MAX))),
            None
        );
    }

    #[test]
    fn fixed_ring_recipe_does_not_panic_or_claim_reexecution_on_pair_override() {
        let base = opaque_pair("base", crate::LinearRgba::BLACK);
        let candidate = opaque_pair("candidate", crate::LinearRgba::BLACK);
        let fixed_ring_recipe = DerivationRecipe {
            name: "fixed_ring",
            bindings: vec![
                crate::RecipeBinding::Colour {
                    name: "ring".into(),
                    value: crate::LinearRgba::WHITE,
                },
                crate::RecipeBinding::Pair {
                    name: "base".into(),
                },
            ],
            implicit_bindings: Vec::new(),
            substitutable_slot: None,
            movement: crate::RecipeMovement::None,
            substitution_domain_constraints: &[],
            output: crate::RecipeOutput::NonText,
            text_contrast_postcondition: false,
            non_text_contrast_postcondition: true,
            opaque_input_precondition: Some(0),
            opaque_output_invariant: true,
            substitution_policy: None,
        };
        let cell = crate::ResolvedButtonCell {
            pair_name: "base".into(),
            pair: base.clone(),
            pair_recipe: None,
            border_name: None,
            border: None,
            ring_name: Some("ring".into()),
            ring: Some(crate::LinearRgba::WHITE),
            ring_recipe: Some(fixed_ring_recipe),
            ring_provenance: None,
            height: 0.0,
            min_width: 0.0,
            padding_x: 0.0,
            border_width: 0.0,
            radius: 0.0,
        };
        let tables = ResolvedTables {
            button: ResolvedButtonTable::new(vec![cell; crate::BUTTON_CELL_COUNT]),
        };
        let dictionary = ResolvedDictionary {
            colours: ResolvedColours {
                pairs: BTreeMap::from([
                    ("base".into(), base),
                    ("candidate".into(), candidate.clone()),
                ]),
                ..Default::default()
            },
            ..Default::default()
        };
        let key = ButtonCellKey {
            variant: crate::ButtonVariant::Default,
            size: crate::ButtonSize::Md,
            interaction: crate::InteractionState::Resting,
            focus_visible: false,
        };

        match button_pair_override(&tables, &dictionary, key, "candidate") {
            PairOverrideDisposition::Available(PairOverrideRoute::ReplaceWhole {
                pair,
                ring_recipe,
            }) => {
                assert_eq!(pair, &candidate);
                assert_eq!(ring_recipe, None);
            }
            actual => panic!("fixed ring must leave a whole-pair route available: {actual:?}"),
        }
    }
}
