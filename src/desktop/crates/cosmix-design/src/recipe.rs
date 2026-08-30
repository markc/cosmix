use std::collections::BTreeMap;
#[cfg(any(feature = "compiler", test))]
use std::collections::BTreeSet;

use crate::{LinearRgba, Mode};

/// The authored input kind accepted by a registered derivation parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeParam {
    Pair,
    Colour,
    ColourList,
    Ratio,
}

/// A minimum collection size attached to one authored parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipeCardinalityConstraint {
    pub param_index: usize,
    pub minimum: usize,
}

/// Domain admitted when the product walker hypothetically substitutes a pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipePairDomain {
    NonTransparentSurface,
}

impl RecipePairDomain {
    pub(crate) fn admits_surface_alpha(self, alpha: f64) -> bool {
        match self {
            Self::NonTransparentSurface => alpha > 0.0,
        }
    }

    pub(crate) const fn description(self) -> &'static str {
        match self {
            Self::NonTransparentSurface => "a non-transparent surface",
        }
    }
}

/// A domain restriction on a substitutable pair argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipeSubstitutionDomainConstraint {
    pub param_index: usize,
    pub domain: RecipePairDomain,
}

/// Why one pair cannot occupy a retained recipe's substitutable slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairRefExclusion {
    OutsideRecipeDomain { required: RecipePairDomain },
}

/// The compile-time disposition of one closed-vocabulary pair reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairRefDecision {
    Admitted,
    Excluded(PairRefExclusion),
}

/// A total, compiled classification of the pair dictionary for one slot.
///
/// Fields and construction are private so only the checked compiler path can
/// create a policy. Consumers can query it but cannot install an incomplete
/// classification into a resolved artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairSubstitutionPolicy {
    slot: usize,
    decisions: BTreeMap<String, PairRefDecision>,
}

impl PairSubstitutionPolicy {
    #[cfg(any(feature = "compiler", test))]
    fn new(
        slot: usize,
        decisions: BTreeMap<String, PairRefDecision>,
        expected_pair_names: &BTreeSet<String>,
    ) -> Result<Self, String> {
        let actual_pair_names = decisions.keys().cloned().collect::<BTreeSet<_>>();
        if actual_pair_names != *expected_pair_names {
            return Err(format!(
                "pair substitution policy keys {actual_pair_names:?} do not equal the resolved pair dictionary keys {expected_pair_names:?}"
            ));
        }
        Ok(Self { slot, decisions })
    }

    pub const fn slot(&self) -> usize {
        self.slot
    }

    pub fn decision(&self, pair_name: &str) -> Option<&PairRefDecision> {
        self.decisions.get(pair_name)
    }

    pub fn decisions(&self) -> impl Iterator<Item = (&str, &PairRefDecision)> {
        self.decisions
            .iter()
            .map(|(name, decision)| (name.as_str(), decision))
    }
}

/// A compiler-supplied input which is retained but absent from authored arity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeImplicitInput {
    ContextMode,
}

/// The resolved property kind produced by a registered derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeOutput {
    Pair,
    NonText,
}

/// The delivered pair member a base-pair derivation promises to move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeMovement {
    None,
    Surface,
    Foreground,
}

/// Data contract for a compiler-registered derivation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecipeSignature {
    pub name: &'static str,
    pub params: &'static [RecipeParam],
    pub cardinality_constraints: &'static [RecipeCardinalityConstraint],
    pub substitution_domain_constraints: &'static [RecipeSubstitutionDomainConstraint],
    pub implicit_inputs: &'static [RecipeImplicitInput],
    /// Index in the signature's authored-then-implicit logical input list.
    /// A valid slot must land in the authored prefix; indexing the complete
    /// list lets signature validation distinguish an implicit slot from an
    /// entirely out-of-range slot instead of conflating the two faults.
    /// Marks override-triggered re-execution only. `None` does not make the
    /// eager output immutable: a direct property override still replaces it
    /// whole under SPEC 19 §9.2.
    pub substitutable_slot: Option<usize>,
    pub movement: RecipeMovement,
    pub output: RecipeOutput,
    pub text_contrast_postcondition: bool,
    pub non_text_contrast_postcondition: bool,
    /// Authored colour argument which must resolve opaque before evaluation.
    pub opaque_input_precondition: Option<usize>,
    /// Whether every produced value must be checked opaque after evaluation.
    pub opaque_output_invariant: bool,
}

const NO_IMPLICIT_INPUTS: &[RecipeImplicitInput] = &[];
const NO_CARDINALITY_CONSTRAINTS: &[RecipeCardinalityConstraint] = &[];
const NO_SUBSTITUTION_DOMAIN_CONSTRAINTS: &[RecipeSubstitutionDomainConstraint] = &[];
const NON_TRANSPARENT_PAIR_SLOT_ZERO: &[RecipeSubstitutionDomainConstraint] =
    &[RecipeSubstitutionDomainConstraint {
        param_index: 0,
        domain: RecipePairDomain::NonTransparentSurface,
    }];
const CONTEXT_MODE_INPUT: &[RecipeImplicitInput] = &[RecipeImplicitInput::ContextMode];
const CONTRAST_SAFE_LIFT_PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
const CONTRAST_SAFE_TOWARD_PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
/// `background.3` supplies L, `accent.default` supplies C,
/// `background.1` supplies H, and `foreground.default` supplies the paired
/// foreground. The evaluator owns the fixed +0.03 / x0.35 / 0.08 cap model;
/// they are deliberately not authored expression operands.
const CONTROL_PAIR_PARAMS: &[RecipeParam] = &[
    RecipeParam::Colour,
    RecipeParam::Colour,
    RecipeParam::Colour,
    RecipeParam::Colour,
];
const SELECTION_PAIR_PARAMS: &[RecipeParam] = &[RecipeParam::Colour, RecipeParam::Colour];
// Pair, maximum absolute OKLCH lightness travel, then proportional chroma
// reduction in the closed interval 0..=1.
const DISABLED_PAIR_PARAMS: &[RecipeParam] =
    &[RecipeParam::Pair, RecipeParam::Ratio, RecipeParam::Ratio];
const CONTRAST_SAFE_STATE_PAIR_PARAMS: &[RecipeParam] = &[
    RecipeParam::Pair,
    RecipeParam::ColourList,
    RecipeParam::Ratio,
];
const FOCUS_RING_PARAMS: &[RecipeParam] = &[RecipeParam::Colour, RecipeParam::Pair];
const CONTRAST_SAFE_STATE_PAIR_CARDINALITY: &[RecipeCardinalityConstraint] =
    &[RecipeCardinalityConstraint {
        param_index: 1,
        minimum: 1,
    }];

/// The complete derivation registry for this compiler revision.
pub const REGISTRY: &[RecipeSignature] = &[
    RecipeSignature {
        name: "contrast_safe_lift",
        params: CONTRAST_SAFE_LIFT_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NON_TRANSPARENT_PAIR_SLOT_ZERO,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: Some(0),
        movement: RecipeMovement::Surface,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "contrast_safe_toward",
        params: CONTRAST_SAFE_TOWARD_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NON_TRANSPARENT_PAIR_SLOT_ZERO,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: Some(0),
        movement: RecipeMovement::Surface,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "control_pair",
        params: CONTROL_PAIR_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: None,
        movement: RecipeMovement::None,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "selection_pair",
        params: SELECTION_PAIR_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
        implicit_inputs: CONTEXT_MODE_INPUT,
        substitutable_slot: None,
        movement: RecipeMovement::None,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "disabled_pair",
        params: DISABLED_PAIR_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NON_TRANSPARENT_PAIR_SLOT_ZERO,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: Some(0),
        // The registered non-transparent-surface domain means every admitted
        // call takes the surface walker. The evaluator's foreground fallback
        // exists for direct model use with alpha-zero surfaces, but it is not
        // part of this registered recipe contract.
        movement: RecipeMovement::Surface,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "contrast_safe_state_pair",
        params: CONTRAST_SAFE_STATE_PAIR_PARAMS,
        cardinality_constraints: CONTRAST_SAFE_STATE_PAIR_CARDINALITY,
        substitution_domain_constraints: NON_TRANSPARENT_PAIR_SLOT_ZERO,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: Some(0),
        movement: RecipeMovement::Surface,
        output: RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
    },
    RecipeSignature {
        name: "focus_ring",
        params: FOCUS_RING_PARAMS,
        cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
        substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
        implicit_inputs: NO_IMPLICIT_INPUTS,
        substitutable_slot: Some(1),
        movement: RecipeMovement::None,
        output: RecipeOutput::NonText,
        text_contrast_postcondition: false,
        non_text_contrast_postcondition: true,
        opaque_input_precondition: Some(0),
        opaque_output_invariant: true,
    },
];

/// A typed, resolved authored input retained for recipe re-execution.
#[derive(Clone, Debug, PartialEq)]
pub enum RecipeBinding {
    Pair {
        name: String,
    },
    Colour {
        name: String,
        value: LinearRgba,
    },
    ColourList {
        names: Vec<String>,
        values: Vec<LinearRgba>,
    },
    Ratio {
        name: String,
        value: f64,
    },
}

impl RecipeBinding {
    pub const fn param(&self) -> RecipeParam {
        match self {
            Self::Pair { .. } => RecipeParam::Pair,
            Self::Colour { .. } => RecipeParam::Colour,
            Self::ColourList { .. } => RecipeParam::ColourList,
            Self::Ratio { .. } => RecipeParam::Ratio,
        }
    }
}

/// A resolved compiler-supplied input retained independently of authored args.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecipeImplicitBinding {
    ContextMode(Mode),
}

impl RecipeImplicitBinding {
    pub const fn input(self) -> RecipeImplicitInput {
        match self {
            Self::ContextMode(_) => RecipeImplicitInput::ContextMode,
        }
    }
}

/// A compiled derivation with no dependency on raw source arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivationRecipe {
    pub name: &'static str,
    pub bindings: Vec<RecipeBinding>,
    pub implicit_bindings: Vec<RecipeImplicitBinding>,
    pub substitutable_slot: Option<usize>,
    pub movement: RecipeMovement,
    pub substitution_domain_constraints: &'static [RecipeSubstitutionDomainConstraint],
    pub output: RecipeOutput,
    pub text_contrast_postcondition: bool,
    pub non_text_contrast_postcondition: bool,
    pub opaque_input_precondition: Option<usize>,
    pub opaque_output_invariant: bool,
    pub(crate) substitution_policy: Option<PairSubstitutionPolicy>,
}

impl DerivationRecipe {
    pub fn substitution_policy(&self) -> Option<&PairSubstitutionPolicy> {
        self.substitution_policy.as_ref()
    }

    #[cfg(feature = "compiler")]
    pub(crate) fn with_substitution_policy(
        mut self,
        slot: usize,
        decisions: BTreeMap<String, PairRefDecision>,
        expected_pair_names: &BTreeSet<String>,
    ) -> Result<Self, String> {
        self.substitution_policy = Some(PairSubstitutionPolicy::new(
            slot,
            decisions,
            expected_pair_names,
        )?);
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "compiler")]
    use crate::recipe_compiler::validate_recipe_registry;

    #[test]
    fn registry_pins_the_b3_rows_and_substitution_contracts() {
        assert_eq!(
            REGISTRY.iter().map(|row| row.name).collect::<Vec<_>>(),
            [
                "contrast_safe_lift",
                "contrast_safe_toward",
                "control_pair",
                "selection_pair",
                "disabled_pair",
                "contrast_safe_state_pair",
                "focus_ring",
            ]
        );
        assert_eq!(
            REGISTRY
                .iter()
                .map(|row| (row.name, row.movement))
                .collect::<Vec<_>>(),
            [
                ("contrast_safe_lift", RecipeMovement::Surface),
                ("contrast_safe_toward", RecipeMovement::Surface),
                ("control_pair", RecipeMovement::None),
                ("selection_pair", RecipeMovement::None),
                ("disabled_pair", RecipeMovement::Surface),
                ("contrast_safe_state_pair", RecipeMovement::Surface),
                ("focus_ring", RecipeMovement::None),
            ]
        );
        let selection = REGISTRY
            .iter()
            .find(|row| row.name == "selection_pair")
            .unwrap();
        assert_eq!(
            selection.implicit_inputs,
            [RecipeImplicitInput::ContextMode]
        );
        assert_eq!(selection.substitutable_slot, None);
        let disabled = REGISTRY
            .iter()
            .find(|row| row.name == "disabled_pair")
            .unwrap();
        assert_eq!(
            disabled.params,
            [RecipeParam::Pair, RecipeParam::Ratio, RecipeParam::Ratio]
        );
        assert_eq!(disabled.substitutable_slot, Some(0));
        assert_eq!(
            disabled.substitution_domain_constraints,
            NON_TRANSPARENT_PAIR_SLOT_ZERO
        );
        let toward = REGISTRY
            .iter()
            .find(|row| row.name == "contrast_safe_toward")
            .unwrap();
        assert_eq!(
            toward.substitution_domain_constraints,
            NON_TRANSPARENT_PAIR_SLOT_ZERO
        );
        let state = REGISTRY
            .iter()
            .find(|row| row.name == "contrast_safe_state_pair")
            .unwrap();
        assert_eq!(state.substitutable_slot, Some(0));
        assert_eq!(
            state.substitution_domain_constraints,
            NON_TRANSPARENT_PAIR_SLOT_ZERO
        );
        assert_eq!(
            state.cardinality_constraints,
            [RecipeCardinalityConstraint {
                param_index: 1,
                minimum: 1,
            }]
        );
        let focus_ring = REGISTRY
            .iter()
            .find(|row| row.name == "focus_ring")
            .unwrap();
        assert_eq!(focus_ring.params, [RecipeParam::Colour, RecipeParam::Pair]);
        assert_eq!(focus_ring.substitutable_slot, Some(1));
        assert_eq!(focus_ring.output, RecipeOutput::NonText);
        assert!(!focus_ring.text_contrast_postcondition);
        assert!(focus_ring.non_text_contrast_postcondition);
        assert_eq!(focus_ring.opaque_input_precondition, Some(0));
        assert!(focus_ring.opaque_output_invariant);
        assert!(
            REGISTRY
                .iter()
                .filter(|row| row.name != "focus_ring")
                .all(|row| row.output == RecipeOutput::Pair
                    && row.text_contrast_postcondition
                    && !row.non_text_contrast_postcondition)
        );
    }

    #[test]
    #[cfg(feature = "compiler")]
    fn every_registry_row_matches_structural_and_evaluator_law() {
        let mut errors = Vec::new();
        assert!(validate_recipe_registry(
            REGISTRY,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.is_empty());

        let structurally_bad_row = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(1),
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        assert!(!validate_recipe_registry(
            &structurally_bad_row,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic.code == "invalid-derivation-signature"
                && diagnostic.message.contains("not a pair")
        }));

        let evaluator_implicit_mismatch = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: CONTEXT_MODE_INPUT,
            substitutable_slot: Some(0),
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        let valid = validate_recipe_registry(
            &evaluator_implicit_mismatch,
            "compiler.derivations",
            &mut errors,
        );
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("declared inputs do not match its compiled evaluator")
        }));
        assert!(!valid);

        let evaluator_params_mismatch = [RecipeSignature {
            name: "contrast_safe_lift",
            params: &[RecipeParam::Pair, RecipeParam::Colour],
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        let valid = validate_recipe_registry(
            &evaluator_params_mismatch,
            "compiler.derivations",
            &mut errors,
        );
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("declared inputs do not match its compiled evaluator")
        }));
        assert!(!valid);

        let invalid_cardinality_constraint = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: &[RecipeCardinalityConstraint {
                param_index: 1,
                minimum: 1,
            }],
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        let valid = validate_recipe_registry(
            &invalid_cardinality_constraint,
            "compiler.derivations",
            &mut errors,
        );
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("cardinality constraint on a non-list argument")
        }));
        assert!(!valid);

        let invalid_domain_constraint = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: &[RecipeSubstitutionDomainConstraint {
                param_index: 1,
                domain: RecipePairDomain::NonTransparentSurface,
            }],
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        let valid = validate_recipe_registry(
            &invalid_domain_constraint,
            "compiler.derivations",
            &mut errors,
        );
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("domain constraint that does not target its substitutable slot")
        }));
        assert!(!valid);
    }

    #[test]
    #[cfg(feature = "compiler")]
    fn registry_validation_rejects_missing_spurious_and_evaluator_drifted_movement() {
        let base_without_movement = [RecipeSignature {
            name: "future_pair_recipe",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: RecipeMovement::None,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        assert!(!validate_recipe_registry(
            &base_without_movement,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("binds a base pair but declares no movement")
        }));

        let moving_without_slot = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: None,
            movement: RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        assert!(!validate_recipe_registry(
            &moving_without_slot,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("declares movement without a substitutable base-pair slot")
        }));

        let constructor_with_movement = [RecipeSignature {
            name: "future_constructor",
            params: SELECTION_PAIR_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: None,
            movement: RecipeMovement::Foreground,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        assert!(!validate_recipe_registry(
            &constructor_with_movement,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("constructor without a base pair but declares movement")
        }));

        let evaluator_movement_mismatch = [RecipeSignature {
            name: "contrast_safe_lift",
            params: CONTRAST_SAFE_LIFT_PARAMS,
            cardinality_constraints: NO_CARDINALITY_CONSTRAINTS,
            substitution_domain_constraints: NO_SUBSTITUTION_DOMAIN_CONSTRAINTS,
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: RecipeMovement::Foreground,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let mut errors = Vec::new();
        assert!(!validate_recipe_registry(
            &evaluator_movement_mismatch,
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("declared movement does not match its compiled evaluator")
        }));
    }

    #[test]
    #[cfg(feature = "compiler")]
    fn focus_ring_seed_slot_cannot_be_declared_substitutable() {
        let mut bad = *REGISTRY
            .iter()
            .find(|signature| signature.name == "focus_ring")
            .unwrap();
        bad.substitutable_slot = Some(0);
        let mut errors = Vec::new();
        assert!(!validate_recipe_registry(
            &[bad],
            "compiler.derivations",
            &mut errors
        ));
        assert!(errors.iter().any(|diagnostic| {
            diagnostic.code == "invalid-derivation-signature"
                && diagnostic.message.contains("not a pair")
        }));
    }

    /// The artifact query tells a cell-owned derivation apart from a plain
    /// reference to a palette-derived pair by comparing the painted pair
    /// against its source dictionary entry. That comparison only decides the
    /// question because a substitutable pair recipe is guaranteed to move a
    /// delivered member; a row that kept the slot and dropped the movement
    /// would evaluate to its own base and be misread as a plain reference.
    ///
    /// Swept over the real registry rather than provoked by mutating one row:
    /// no registered row pairs `movement: None` with a pair-typed substitutable
    /// param, so mutating an existing signature trips the evaluator-contract
    /// check first and never reaches the invariant. A new row that declared
    /// both would, which is precisely the drift this pins.
    #[test]
    fn every_substitutable_pair_recipe_declares_delivered_movement() {
        let substitutable_pairs: Vec<_> = REGISTRY
            .iter()
            .filter(|signature| {
                signature.output == RecipeOutput::Pair && signature.substitutable_slot.is_some()
            })
            .collect();
        assert!(
            !substitutable_pairs.is_empty(),
            "the sweep must find rows or it proves nothing"
        );
        for signature in substitutable_pairs {
            assert_ne!(
                signature.movement,
                RecipeMovement::None,
                "`{}` retains a substitutable pair slot but declares no delivered movement, \
                 which would let a cell-owned derivation evaluate to its own base and be \
                 misrouted as a plain dictionary reference by the artifact query",
                signature.name
            );
        }
    }

    #[test]
    fn pair_policy_constructor_rejects_dictionary_key_drift() {
        let decisions = BTreeMap::from([("base".into(), PairRefDecision::Admitted)]);
        let expected = BTreeSet::from(["base".into(), "muted".into()]);
        let error = PairSubstitutionPolicy::new(0, decisions, &expected).unwrap_err();
        assert!(error.contains("do not equal the resolved pair dictionary keys"));
    }
}
