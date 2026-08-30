use std::collections::{BTreeMap, BTreeSet};

use crate::recipe::DerivationRecipe;

/// The closed §2.4 semantic vocabulary. These are model facts, not compiler
/// internals: they are the exact keys of [`ResolvedColours::pairs`] and
/// [`ResolvedColours::non_text`], so a consumer indexing a resolved
/// artifact needs them as much as the compiler that fills it does.
pub const TEXT_PAIR_NAMES: [&str; 8] = [
    "base",
    "card",
    "popover",
    "primary",
    "secondary",
    "muted",
    "accent",
    "destructive",
];
pub const NON_TEXT_NAMES: [&str; 3] = ["border", "input", "ring"];

/// Linear-light sRGB plus alpha, used for all compiler measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearRgba {
    pub red: f64,
    pub green: f64,
    pub blue: f64,
    pub alpha: f64,
}

/// Observable trace data for one SPEC 19 §3.5.2.1 focus-ring walk.
#[derive(Clone, Debug, PartialEq)]
pub struct FocusRingProvenance {
    pub seed_name: String,
    pub step_index: u32,
    pub delta_l: f64,
}

impl LinearRgba {
    pub const BLACK: Self = Self {
        red: 0.0,
        green: 0.0,
        blue: 0.0,
        alpha: 1.0,
    };
    pub const WHITE: Self = Self {
        red: 1.0,
        green: 1.0,
        blue: 1.0,
        alpha: 1.0,
    };

    pub fn to_srgba8(self) -> [u8; 4] {
        [
            quantise(linear_to_srgb(self.red)),
            quantise(linear_to_srgb(self.green)),
            quantise(linear_to_srgb(self.blue)),
            quantise(self.alpha),
        ]
    }

    #[cfg_attr(not(feature = "compiler"), allow(dead_code))]
    pub(crate) fn opaque(self) -> bool {
        // SPEC 19's opaque precondition means exactly 1.0; an epsilon would
        // silently admit a directly authored translucent input. A composited
        // alpha can still round a true value just below 1.0 up to exactly 1.0;
        // the bounded, accepted residual is pinned in the tests below.
        self.alpha == 1.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPair {
    pub surface_name: String,
    pub surface: LinearRgba,
    pub foreground_name: String,
    pub foreground: LinearRgba,
    pub backdrop_name: Option<String>,
    pub backdrop: Option<LinearRgba>,
    pub rendered_surface: LinearRgba,
    pub rendered_foreground: LinearRgba,
    pub contrast_ratio: f64,
    /// Present when the semantic pair itself was produced by a registered
    /// derivation. Mapping cells also retain their immediate recipe; keeping
    /// this origin on the dictionary value prevents a derived semantic token
    /// from becoming indistinguishable from an authored one during tracing.
    pub recipe: Option<DerivationRecipe>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedNonTextColour {
    pub value_name: String,
    pub value: LinearRgba,
    pub adjacent: BTreeSet<String>,
}

/// Compiled colour output. Every member is resolved — no source type survives
/// into it — so it belongs to the model rather than the compiler, and a
/// consumer built without the `compiler` feature can still accept a whole
/// compiled colour artifact.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedColours {
    pub primitives: BTreeMap<String, LinearRgba>,
    pub pairs: BTreeMap<String, ResolvedPair>,
    pub non_text: BTreeMap<String, ResolvedNonTextColour>,
}

pub fn contrast_ratio(a: LinearRgba, b: LinearRgba) -> f64 {
    let light = relative_luminance(a).max(relative_luminance(b));
    let dark = relative_luminance(a).min(relative_luminance(b));
    (light + 0.05) / (dark + 0.05)
}

fn relative_luminance(value: LinearRgba) -> f64 {
    0.2126 * value.red + 0.7152 * value.green + 0.0722 * value.blue
}

// Derivation maths stays model-side because SPEC 19 §10.2 requires
// resolver-time re-execution under instance overrides. It remains crate-private
// today because no external resolver calls it yet; public exposure arrives with
// the override work. The dead-code suppression only covers model-only builds.
#[cfg_attr(not(feature = "compiler"), allow(dead_code))]
pub(crate) mod derivation {
    use super::{FocusRingProvenance, LinearRgba, ResolvedColours, ResolvedPair, contrast_ratio};
    use crate::Mode;
    use crate::recipe::{
        DerivationRecipe, RecipeBinding, RecipeImplicitBinding, RecipeImplicitInput,
        RecipeMovement, RecipeOutput, RecipePairDomain, RecipeParam, RecipeSignature,
    };

    const STEPS: u32 = 100;
    const FOCUS_RING_STEPS_PER_LIGHTNESS: f64 = 1000.0;
    const FOCUS_RING_CONTRAST: f64 = 3.0;
    const FOCUS_RING_WARNING_STEP: u32 = 300;
    const TEXT_CONTRAST: f64 = 4.5;
    const SELECTION_SEPARATION: f64 = 7.0;

    #[derive(Clone, Debug)]
    pub(crate) struct RecipeEvaluation {
        pub(crate) pair: ResolvedPair,
        pub(crate) warning: Option<RecipeEvaluationWarning>,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct RecipeEvaluationWarning {
        pub(crate) code: &'static str,
        pub(crate) message: String,
    }

    #[derive(Clone, Debug)]
    pub(crate) struct NonTextRecipeEvaluation {
        pub(crate) value: LinearRgba,
        pub(crate) provenance: FocusRingProvenance,
        pub(crate) warning: Option<RecipeEvaluationWarning>,
    }

    pub(crate) fn evaluate_pair_recipe(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<RecipeEvaluation, String> {
        validate_evaluator_recipe(recipe)?;
        validate_authored_pair_domains(recipe, colours)?;
        let evaluation = match recipe.name {
            "contrast_safe_lift" => match recipe.bindings.as_slice() {
                [
                    RecipeBinding::Pair { name },
                    RecipeBinding::Ratio { value, .. },
                ] => RecipeEvaluation {
                    pair: contrast_safe_lift(pair(colours, name)?, *value)?,
                    warning: None,
                },
                _ => return Err(evaluator_mismatch(recipe)),
            },
            "contrast_safe_toward" => evaluate_contrast_safe_toward(recipe, colours)?,
            "control_pair" => evaluate_control_pair(recipe)?,
            "selection_pair" => evaluate_selection_pair(recipe)?,
            "disabled_pair" => evaluate_disabled_pair(recipe, colours)?,
            "contrast_safe_state_pair" => evaluate_contrast_safe_state_pair(recipe, colours)?,
            _ => {
                return Err(format!(
                    "`{}` is registered but has no compiled evaluator",
                    recipe.name
                ));
            }
        };
        finalize_pair_recipe_evaluation(recipe, colours, evaluation)
    }

    #[cfg(test)]
    pub(crate) fn evaluate_non_text_recipe(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<NonTextRecipeEvaluation, String> {
        validate_evaluator_recipe(recipe)?;
        validate_authored_pair_domains(recipe, colours)?;
        validate_opaque_input_precondition(recipe)?;
        let pair_name = recipe
            .substitutable_slot
            .and_then(|slot| recipe.bindings.get(slot))
            .and_then(|binding| match binding {
                RecipeBinding::Pair { name } => Some(name.as_str()),
                _ => None,
            })
            .ok_or_else(|| evaluator_mismatch(recipe))?;
        let surface = pair(colours, pair_name)?;
        evaluate_non_text_recipe_against(recipe, surface)
    }

    pub(crate) fn evaluate_non_text_recipe_against(
        recipe: &DerivationRecipe,
        surface: &ResolvedPair,
    ) -> Result<NonTextRecipeEvaluation, String> {
        evaluate_non_text_recipe_against_with(recipe, surface, evaluate_non_text_recipe_output)
    }

    fn evaluate_non_text_recipe_against_with<F>(
        recipe: &DerivationRecipe,
        surface: &ResolvedPair,
        mut evaluate: F,
    ) -> Result<NonTextRecipeEvaluation, String>
    where
        F: FnMut(&DerivationRecipe, &ResolvedPair) -> Result<NonTextRecipeEvaluation, String>,
    {
        validate_evaluator_recipe(recipe)?;
        validate_opaque_input_precondition(recipe)?;
        let evaluation = evaluate(recipe, surface)?;
        verify_non_text_postcondition(recipe, evaluation.value, surface.rendered_surface)?;
        Ok(evaluation)
    }

    fn evaluate_non_text_recipe_output(
        recipe: &DerivationRecipe,
        surface: &ResolvedPair,
    ) -> Result<NonTextRecipeEvaluation, String> {
        let evaluation = match recipe.name {
            "focus_ring" => evaluate_focus_ring(recipe, surface)?,
            _ => {
                return Err(format!(
                    "`{}` is registered but has no compiled evaluator",
                    recipe.name
                ));
            }
        };
        Ok(evaluation)
    }

    fn validate_opaque_input_precondition(recipe: &DerivationRecipe) -> Result<(), String> {
        let Some(index) = recipe.opaque_input_precondition else {
            return Ok(());
        };
        let Some(RecipeBinding::Colour { name, value }) = recipe.bindings.get(index) else {
            return Err(evaluator_mismatch(recipe));
        };
        if !value.opaque() {
            return Err(format!(
                "recipe `{}` requires opaque seed colour `{name}` at argument {index}",
                recipe.name
            ));
        }
        Ok(())
    }

    fn evaluate_focus_ring(
        recipe: &DerivationRecipe,
        surface: &ResolvedPair,
    ) -> Result<NonTextRecipeEvaluation, String> {
        let [
            RecipeBinding::Colour {
                name: seed_name,
                value: seed,
            },
            RecipeBinding::Pair { .. },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        let (seed_l, chroma, hue) = linear_srgb_to_oklch(*seed);
        let down =
            focus_ring_direction_first_pass(seed_l, chroma, hue, surface, WalkDirection::Down)?;
        let up = focus_ring_direction_first_pass(seed_l, chroma, hue, surface, WalkDirection::Up)?;
        let chosen = match (down, up) {
            (Some(down), Some(up)) if down.provenance.step_index < up.provenance.step_index => down,
            (Some(_), Some(up)) => up,
            (Some(down), None) => down,
            (None, Some(up)) => up,
            (None, None) => {
                return Err(format!(
                    "recipe `{}` found no 3:1 focus-ring candidate against pair `{}`",
                    recipe.name, surface.surface_name
                ));
            }
        };
        let warning = (chosen.provenance.step_index > FOCUS_RING_WARNING_STEP).then(|| {
            RecipeEvaluationWarning {
                code: "ring-walk-distance",
                message: format!(
                    "focus ring walked from seed `{seed_name}` by step_index {} (delta_l {:.4}) against pair `{}`",
                    chosen.provenance.step_index, chosen.provenance.delta_l, surface.surface_name
                ),
            }
        });
        Ok(NonTextRecipeEvaluation {
            value: chosen.value,
            provenance: FocusRingProvenance {
                seed_name: seed_name.clone(),
                ..chosen.provenance
            },
            warning,
        })
    }

    #[derive(Clone, Copy)]
    enum WalkDirection {
        Down,
        Up,
    }

    #[derive(Clone, Debug)]
    struct FocusRingCandidate {
        value: LinearRgba,
        provenance: FocusRingProvenance,
    }

    fn focus_ring_direction_first_pass(
        seed_l: f64,
        chroma: f64,
        hue: f64,
        surface: &ResolvedPair,
        direction: WalkDirection,
    ) -> Result<Option<FocusRingCandidate>, String> {
        walk_focus_ring_direction(seed_l, chroma, hue, direction, |candidate, _| {
            non_text_contrast_ratio(candidate, surface.rendered_surface) >= FOCUS_RING_CONTRAST
        })
    }

    #[cfg(test)]
    pub(crate) fn focus_ring_direction_totality(
        recipe: &DerivationRecipe,
        surface: &ResolvedPair,
    ) -> Result<(bool, bool), String> {
        let Some(RecipeBinding::Colour { value: seed, .. }) = recipe.bindings.first() else {
            return Err(evaluator_mismatch(recipe));
        };
        let (seed_l, chroma, hue) = linear_srgb_to_oklch(*seed);
        Ok((
            focus_ring_direction_first_pass(seed_l, chroma, hue, surface, WalkDirection::Down)?
                .is_some(),
            focus_ring_direction_first_pass(seed_l, chroma, hue, surface, WalkDirection::Up)?
                .is_some(),
        ))
    }

    fn walk_focus_ring_direction<F>(
        seed_l: f64,
        chroma: f64,
        hue: f64,
        direction: WalkDirection,
        mut passes: F,
    ) -> Result<Option<FocusRingCandidate>, String>
    where
        F: FnMut(LinearRgba, f64) -> bool,
    {
        let endpoint: f64 = match direction {
            WalkDirection::Down => 0.0,
            WalkDirection::Up => 1.0,
        };
        let mut step_index = 0_u32;
        let mut last_lattice_step = 0_u32;
        let mut landed_on_endpoint = false;
        loop {
            let offset = f64::from(step_index) / FOCUS_RING_STEPS_PER_LIGHTNESS;
            let lightness = match direction {
                WalkDirection::Down => seed_l - offset,
                WalkDirection::Up => seed_l + offset,
            };
            if !(0.0..=1.0).contains(&lightness) {
                break;
            }
            let value = oklch_to_linear_srgb(lightness, chroma, hue, 1.0).map_err(str::to_owned)?;
            if passes(value, lightness) {
                return Ok(Some(focus_ring_candidate(
                    value, lightness, seed_l, step_index,
                )));
            }
            last_lattice_step = step_index;
            if lightness.to_bits() == endpoint.to_bits() {
                landed_on_endpoint = true;
                break;
            }
            step_index += 1;
        }
        if landed_on_endpoint {
            return Ok(None);
        }
        let value = oklch_to_linear_srgb(endpoint, chroma, hue, 1.0).map_err(str::to_owned)?;
        Ok(passes(value, endpoint)
            .then(|| focus_ring_candidate(value, endpoint, seed_l, last_lattice_step + 1)))
    }

    fn focus_ring_candidate(
        value: LinearRgba,
        lightness: f64,
        seed_l: f64,
        step_index: u32,
    ) -> FocusRingCandidate {
        FocusRingCandidate {
            value,
            provenance: FocusRingProvenance {
                seed_name: String::new(),
                step_index,
                delta_l: (lightness - seed_l).abs(),
            },
        }
    }

    pub(crate) fn non_text_contrast_ratio(output: LinearRgba, surface: LinearRgba) -> f64 {
        contrast_ratio(composite(output, surface), surface)
    }

    pub(crate) fn verify_non_text_postcondition(
        recipe: &DerivationRecipe,
        output: LinearRgba,
        rendered_surface: LinearRgba,
    ) -> Result<(), String> {
        if recipe.opaque_output_invariant && !output.opaque() {
            return Err(format!(
                "`{}` produced a translucent non-text output; opaque output is invariant",
                recipe.name
            ));
        }
        let ratio = non_text_contrast_ratio(output, rendered_surface);
        if recipe.non_text_contrast_postcondition
            && (!ratio.is_finite() || ratio < FOCUS_RING_CONTRAST)
        {
            return Err(format!(
                "`{}` produced non-text contrast {ratio:.3}:1; expected WCAG 1.4.11 3:1",
                recipe.name
            ));
        }
        Ok(())
    }

    fn finalize_pair_recipe_evaluation(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
        mut evaluation: RecipeEvaluation,
    ) -> Result<RecipeEvaluation, String> {
        enforce_declared_movement(recipe, colours, &evaluation.pair)?;
        evaluation.pair.recipe = Some(recipe.clone());
        Ok(evaluation)
    }

    fn enforce_declared_movement(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
        candidate: &ResolvedPair,
    ) -> Result<(), String> {
        let member = match recipe.movement {
            RecipeMovement::None => return Ok(()),
            member => member,
        };
        let Some(slot) = recipe.substitutable_slot else {
            return Err(evaluator_mismatch(recipe));
        };
        let Some(RecipeBinding::Pair { name }) = recipe.bindings.get(slot) else {
            return Err(evaluator_mismatch(recipe));
        };
        let base = pair(colours, name)?;
        if delivered_member_changed(member, base, candidate) {
            return Ok(());
        }
        let member_name = match member {
            RecipeMovement::Surface => "surface",
            RecipeMovement::Foreground => "foreground",
            RecipeMovement::None => unreachable!("none returned before movement enforcement"),
        };
        Err(format!(
            "recipe `{}` produced no delivered {member_name} byte movement from base pair `{name}`",
            recipe.name
        ))
    }

    fn validate_authored_pair_domains(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<(), String> {
        for constraint in recipe.substitution_domain_constraints {
            let Some(RecipeBinding::Pair { name }) = recipe.bindings.get(constraint.param_index)
            else {
                return Err(evaluator_mismatch(recipe));
            };
            let pair = pair(colours, name)?;
            if constraint.domain.admits_surface_alpha(pair.surface.alpha) {
                continue;
            }
            let cause = match (constraint.domain, recipe.name) {
                (RecipePairDomain::NonTransparentSurface, "contrast_safe_toward") => {
                    "a fully transparent surface cannot move toward its foreground; the foreground must move instead"
                }
                (RecipePairDomain::NonTransparentSurface, _) => {
                    "a fully transparent surface is not admitted for this authored recipe argument"
                }
            };
            return Err(format!(
                "recipe `{}` authored argument {} pair `{name}` is outside its domain requiring {}: {cause}",
                recipe.name,
                constraint.param_index,
                constraint.domain.description()
            ));
        }
        Ok(())
    }

    /// The evaluator's authored and compiler-supplied input contract. Keeping
    /// this independently executable lets registry validation reject a drifted
    /// row even when no source happens to call it.
    pub(crate) fn validate_evaluator_signature(signature: &RecipeSignature) -> Result<(), String> {
        let Some(contract) = evaluator_contract(signature.name) else {
            return Err(format!(
                "`{}` is registered but has no compiled evaluator",
                signature.name
            ));
        };
        if signature.params != contract.params
            || signature.implicit_inputs != contract.implicit_inputs
        {
            return Err(format!(
                "`{}` declared inputs do not match its compiled evaluator",
                signature.name
            ));
        }
        if signature.movement != contract.movement {
            return Err(format!(
                "`{}` declared movement does not match its compiled evaluator",
                signature.name
            ));
        }
        if signature.output != contract.output
            || signature.text_contrast_postcondition != contract.text_contrast_postcondition
            || signature.non_text_contrast_postcondition != contract.non_text_contrast_postcondition
            || signature.opaque_input_precondition != contract.opaque_input_precondition
            || signature.opaque_output_invariant != contract.opaque_output_invariant
        {
            return Err(format!(
                "`{}` declared output obligations do not match its compiled evaluator",
                signature.name
            ));
        }
        // A substitutable pair recipe MUST move a delivered member. The artifact
        // query distinguishes a cell-owned derivation from a plain reference to
        // a palette-derived pair by value inequality against the source
        // dictionary pair (`design_model.rs`), and only enforced movement makes
        // that inequality guaranteed rather than incidental. A substitutable
        // pair recipe declaring no movement would evaluate to its own base and
        // be misrouted as a plain reference, so reject the row here rather than
        // leave the query resting on an unstated coupling. No registered row can
        // reach this today — every pair recipe declaring no movement also has no
        // pair-typed param to make substitutable — so the executable proof is a
        // registry sweep in `recipe.rs`; this arm catches a future evaluator
        // contract that declares both.
        if signature.output == RecipeOutput::Pair
            && signature.substitutable_slot.is_some()
            && signature.movement == RecipeMovement::None
        {
            return Err(format!(
                "`{}` retains a substitutable pair slot but declares no delivered movement",
                signature.name
            ));
        }
        Ok(())
    }

    struct EvaluatorContract {
        params: &'static [RecipeParam],
        implicit_inputs: &'static [RecipeImplicitInput],
        movement: RecipeMovement,
        output: RecipeOutput,
        text_contrast_postcondition: bool,
        non_text_contrast_postcondition: bool,
        opaque_input_precondition: Option<usize>,
        opaque_output_invariant: bool,
    }

    fn pair_contract(
        params: &'static [RecipeParam],
        implicit_inputs: &'static [RecipeImplicitInput],
        movement: RecipeMovement,
    ) -> EvaluatorContract {
        EvaluatorContract {
            params,
            implicit_inputs,
            movement,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }
    }

    fn evaluator_contract(name: &str) -> Option<EvaluatorContract> {
        use RecipeImplicitInput::ContextMode;
        use RecipeParam::{Colour, ColourList, Pair, Ratio};

        match name {
            "contrast_safe_lift" => {
                Some(pair_contract(&[Pair, Ratio], &[], RecipeMovement::Surface))
            }
            "contrast_safe_toward" => {
                Some(pair_contract(&[Pair, Ratio], &[], RecipeMovement::Surface))
            }
            "control_pair" => Some(pair_contract(
                &[Colour, Colour, Colour, Colour],
                &[],
                RecipeMovement::None,
            )),
            "selection_pair" => Some(pair_contract(
                &[Colour, Colour],
                &[ContextMode],
                RecipeMovement::None,
            )),
            "disabled_pair" => Some(pair_contract(
                &[Pair, Ratio, Ratio],
                &[],
                RecipeMovement::Surface,
            )),
            "contrast_safe_state_pair" => Some(pair_contract(
                &[Pair, ColourList, Ratio],
                &[],
                RecipeMovement::Surface,
            )),
            "focus_ring" => Some(EvaluatorContract {
                params: &[Colour, Pair],
                implicit_inputs: &[],
                movement: RecipeMovement::None,
                output: RecipeOutput::NonText,
                text_contrast_postcondition: false,
                non_text_contrast_postcondition: true,
                opaque_input_precondition: Some(0),
                opaque_output_invariant: true,
            }),
            _ => None,
        }
    }

    fn validate_evaluator_recipe(recipe: &DerivationRecipe) -> Result<(), String> {
        let Some(contract) = evaluator_contract(recipe.name) else {
            return Err(format!(
                "`{}` is registered but has no compiled evaluator",
                recipe.name
            ));
        };
        let bindings_match = recipe
            .bindings
            .iter()
            .map(RecipeBinding::param)
            .eq(contract.params.iter().copied());
        let implicit_bindings_match = recipe
            .implicit_bindings
            .iter()
            .copied()
            .map(RecipeImplicitBinding::input)
            .eq(contract.implicit_inputs.iter().copied());
        let declarations_match = recipe.output == contract.output;
        if !bindings_match || !implicit_bindings_match || !declarations_match {
            return Err(evaluator_mismatch(recipe));
        }
        Ok(())
    }

    /// Verifies a registered claim against the value that will actually enter
    /// the cell. Construction-aware evaluators still pass through this gate:
    /// the signature is load-bearing data, not a comment about the evaluator.
    pub(crate) fn verify_text_postcondition(
        recipe: &DerivationRecipe,
        output: &ResolvedPair,
    ) -> Result<(), String> {
        if recipe.text_contrast_postcondition
            && (!output.contrast_ratio.is_finite() || output.contrast_ratio < TEXT_CONTRAST)
        {
            return Err(format!(
                "`{}` produced text contrast {:.3}:1; expected WCAG AA 4.5:1",
                recipe.name, output.contrast_ratio
            ));
        }
        Ok(())
    }

    fn evaluate_control_pair(recipe: &DerivationRecipe) -> Result<RecipeEvaluation, String> {
        let [
            RecipeBinding::Colour {
                name: lightness_name,
                value: lightness_anchor,
            },
            RecipeBinding::Colour {
                value: chroma_anchor,
                ..
            },
            RecipeBinding::Colour {
                value: hue_anchor, ..
            },
            RecipeBinding::Colour {
                name: foreground_name,
                value: foreground,
            },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        if !recipe.implicit_bindings.is_empty() {
            return Err(evaluator_mismatch(recipe));
        }

        // This uses ctk ThemeSpec::from_scheme's control arithmetic:
        // background.3 supplies L, accent.default supplies C, background.1
        // supplies H. Unlike ctk's authored OKLCH anchors, these inputs are
        // delivered, gamut-mapped primitives as required by SPEC 19 §10.8.
        // The constants remain registry law, never source args.
        let (lightness, _, _) = linear_srgb_to_oklch(*lightness_anchor);
        let (_, chroma, _) = linear_srgb_to_oklch(*chroma_anchor);
        let (_, _, hue) = linear_srgb_to_oklch(*hue_anchor);
        let surface = oklch_to_linear_srgb(lightness + 0.03, (chroma * 0.35).min(0.08), hue, 1.0)
            .map_err(str::to_owned)?;
        Ok(RecipeEvaluation {
            pair: resolved_pair(
                format!("derive:control_pair({lightness_name})"),
                surface,
                foreground_name.clone(),
                *foreground,
                None,
                None,
            )?,
            warning: None,
        })
    }

    fn evaluate_selection_pair(recipe: &DerivationRecipe) -> Result<RecipeEvaluation, String> {
        let [
            RecipeBinding::Colour {
                name: accent_name,
                value: accent,
            },
            RecipeBinding::Colour {
                name: panel_name,
                value: panel,
            },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        let [RecipeImplicitBinding::ContextMode(mode)] = recipe.implicit_bindings.as_slice() else {
            return Err(evaluator_mismatch(recipe));
        };
        if !panel.opaque() {
            return Err("selection_pair requires an opaque panel colour".into());
        }

        // This uses ctk's `separated_from` followed by `contrast_checked` at
        // SELECTION_SEPARATION. Unlike ctk's authored OKLCH anchor, the accent
        // input is the delivered, gamut-mapped primitive required by SPEC 19
        // §10.8; evaluation and contrast therefore use delivered colours.
        let (accent_l, accent_c, accent_h) = linear_srgb_to_oklch(*accent);
        let seed_c = (accent_c * 0.72).min(0.16);
        let surface_extreme = match mode {
            Mode::Dark => 1.0,
            Mode::Light => 0.0,
        };
        let mut selected = None;
        for step in 0..=STEPS {
            let t = f64::from(step) / f64::from(STEPS);
            let candidate = oklch_to_linear_srgb(
                accent_l + (surface_extreme - accent_l) * t,
                seed_c,
                accent_h,
                1.0,
            )
            .map_err(str::to_owned)?;
            if contrast_ratio(candidate, *panel) >= SELECTION_SEPARATION {
                selected = Some(candidate);
                break;
            }
        }
        let (surface, warning) = match selected {
            Some(surface) => (surface, None),
            None => (
                oklch_to_linear_srgb(surface_extreme, seed_c, accent_h, 1.0)
                    .map_err(str::to_owned)?,
                Some(RecipeEvaluationWarning {
                    code: "selection-separation",
                    message: format!(
                        "selection separation target {SELECTION_SEPARATION}:1 is unreachable; using the {mode:?} mode extreme as a best-effort fallback"
                    ),
                }),
            ),
        };

        let (panel_l, panel_c, panel_h) = linear_srgb_to_oklch(*panel);
        let (preferred, fallback) = match mode {
            Mode::Dark => (0.0, 1.0),
            Mode::Light => (1.0, 0.0),
        };
        let foreground = [preferred, fallback]
            .into_iter()
            .find_map(|toward| {
                (0..=STEPS).find_map(|step| {
                    let t = f64::from(step) / f64::from(STEPS);
                    let candidate = oklch_to_linear_srgb(
                        panel_l + (toward - panel_l) * t,
                        panel_c * (1.0 - t),
                        panel_h,
                        1.0,
                    )
                    .ok()?;
                    (contrast_ratio(candidate, surface) >= TEXT_CONTRAST).then_some(candidate)
                })
            })
            .unwrap_or_else(|| {
                if contrast_ratio(LinearRgba::BLACK, surface)
                    >= contrast_ratio(LinearRgba::WHITE, surface)
                {
                    LinearRgba::BLACK
                } else {
                    LinearRgba::WHITE
                }
            });

        Ok(RecipeEvaluation {
            pair: resolved_pair(
                format!("derive:selection_pair({accent_name})"),
                surface,
                format!("derive:selection_pair({panel_name}).foreground"),
                foreground,
                None,
                None,
            )?,
            warning,
        })
    }

    fn evaluate_contrast_safe_toward(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<RecipeEvaluation, String> {
        let [
            RecipeBinding::Pair { name },
            RecipeBinding::Ratio { value: lift, .. },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        if !recipe.implicit_bindings.is_empty() || !lift.is_finite() || *lift < 0.0 {
            return Err(evaluator_mismatch(recipe));
        }
        let base = pair(colours, name)?;
        Ok(RecipeEvaluation {
            pair: surface_toward_foreground(name, base, *lift)?,
            warning: None,
        })
    }

    fn evaluate_disabled_pair(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<RecipeEvaluation, String> {
        let [
            RecipeBinding::Pair { name },
            RecipeBinding::Ratio { value: lift, .. },
            RecipeBinding::Ratio {
                value: chroma_reduction,
                ..
            },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        if !recipe.implicit_bindings.is_empty()
            || !lift.is_finite()
            || *lift < 0.0
            || !chroma_reduction.is_finite()
            || !(0.0..=1.0).contains(chroma_reduction)
        {
            return Err(evaluator_mismatch(recipe));
        }
        let base = pair(colours, name)?;
        let pair = if base.surface.alpha == 0.0 {
            disabled_foreground_toward_rendered_surface(name, base, *lift, *chroma_reduction)?
        } else {
            disabled_surface_toward_foreground(name, base, *lift, *chroma_reduction)?
        };
        Ok(RecipeEvaluation {
            pair,
            warning: None,
        })
    }

    fn surface_toward_foreground(
        pair_name: &str,
        base: &ResolvedPair,
        lift: f64,
    ) -> Result<ResolvedPair, String> {
        if base.surface.alpha == 0.0 {
            return Err(format!(
                "pair `{pair_name}` has a fully transparent surface, which cannot move toward its foreground; the foreground must move instead"
            ));
        }
        let (surface_l, surface_c, surface_h) = linear_srgb_to_oklch(base.surface);
        let (foreground_l, _, _) = linear_srgb_to_oklch(base.foreground);
        let toward_sign = if surface_l >= foreground_l { -1.0 } else { 1.0 };
        let target_distance = (foreground_l - surface_l).abs();
        for step in (1..=STEPS).rev() {
            let travel = (lift * f64::from(step) / f64::from(STEPS)).min(target_distance);
            let Ok(surface) = oklch_to_linear_srgb(
                surface_l + travel * toward_sign,
                surface_c,
                surface_h,
                base.surface.alpha,
            ) else {
                continue;
            };
            let Ok(candidate) = resolved_pair(
                base.surface_name.clone(),
                surface,
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            ) else {
                continue;
            };
            if candidate.contrast_ratio >= TEXT_CONTRAST
                && delivered_member_changed(RecipeMovement::Surface, base, &candidate)
            {
                return Ok(candidate);
            }
        }
        Err(format!(
            "pair `{pair_name}` cannot move toward its foreground by non-zero ratio {lift} while retaining WCAG AA"
        ))
    }

    fn disabled_surface_toward_foreground(
        pair_name: &str,
        base: &ResolvedPair,
        lift: f64,
        chroma_reduction: f64,
    ) -> Result<ResolvedPair, String> {
        let (surface_l, surface_c, surface_h) = linear_srgb_to_oklch(base.surface);
        let (foreground_l, _, _) = linear_srgb_to_oklch(base.foreground);
        let toward_sign = if surface_l >= foreground_l { -1.0 } else { 1.0 };
        let target_distance = (foreground_l - surface_l).abs();
        for step in (1..=STEPS).rev() {
            let fraction = f64::from(step) / f64::from(STEPS);
            let travel = (lift * fraction).min(target_distance);
            let chroma = surface_c * (1.0 - chroma_reduction * fraction);
            let Ok(surface) = oklch_to_linear_srgb(
                surface_l + travel * toward_sign,
                chroma,
                surface_h,
                base.surface.alpha,
            ) else {
                continue;
            };
            let Ok(candidate) = resolved_pair(
                base.surface_name.clone(),
                surface,
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            ) else {
                continue;
            };
            if candidate.contrast_ratio >= TEXT_CONTRAST
                && delivered_member_changed(RecipeMovement::Surface, base, &candidate)
            {
                return Ok(candidate);
            }
        }
        Err(format!(
            "pair `{pair_name}` cannot move its disabled surface by non-zero lightness ratio {lift} and chroma reduction {chroma_reduction} while retaining WCAG AA"
        ))
    }

    fn disabled_foreground_toward_rendered_surface(
        pair_name: &str,
        base: &ResolvedPair,
        lift: f64,
        chroma_reduction: f64,
    ) -> Result<ResolvedPair, String> {
        if base.backdrop.is_none() {
            return Err("a fully transparent disabled surface requires a backdrop".into());
        }
        let (foreground_l, foreground_c, foreground_h) = linear_srgb_to_oklch(base.foreground);
        let (surface_l, _, _) = linear_srgb_to_oklch(base.rendered_surface);
        let toward_sign = if foreground_l >= surface_l { -1.0 } else { 1.0 };
        let target_distance = (surface_l - foreground_l).abs();
        for step in (1..=STEPS).rev() {
            let fraction = f64::from(step) / f64::from(STEPS);
            let travel = (lift * fraction).min(target_distance);
            let chroma = foreground_c * (1.0 - chroma_reduction * fraction);
            let Ok(foreground) = oklch_to_linear_srgb(
                foreground_l + travel * toward_sign,
                chroma,
                foreground_h,
                base.foreground.alpha,
            ) else {
                continue;
            };
            let Ok(candidate) = resolved_pair(
                base.surface_name.clone(),
                base.surface,
                base.foreground_name.clone(),
                foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            ) else {
                continue;
            };
            if candidate.contrast_ratio >= TEXT_CONTRAST
                && delivered_member_changed(RecipeMovement::Foreground, base, &candidate)
            {
                return Ok(candidate);
            }
        }
        Err(format!(
            "pair `{pair_name}` cannot move its disabled foreground by non-zero lightness ratio {lift} and chroma reduction {chroma_reduction} while retaining WCAG AA"
        ))
    }

    fn delivered_member_changed(
        member: RecipeMovement,
        base: &ResolvedPair,
        candidate: &ResolvedPair,
    ) -> bool {
        match member {
            RecipeMovement::Surface => {
                base.rendered_surface.to_srgba8() != candidate.rendered_surface.to_srgba8()
            }
            RecipeMovement::Foreground => {
                base.rendered_foreground.to_srgba8() != candidate.rendered_foreground.to_srgba8()
            }
            RecipeMovement::None => false,
        }
    }

    fn evaluate_contrast_safe_state_pair(
        recipe: &DerivationRecipe,
        colours: &ResolvedColours,
    ) -> Result<RecipeEvaluation, String> {
        let [
            RecipeBinding::Pair { name },
            RecipeBinding::ColourList { values: extra, .. },
            RecipeBinding::Ratio { value: lift, .. },
        ] = recipe.bindings.as_slice()
        else {
            return Err(evaluator_mismatch(recipe));
        };
        if !recipe.implicit_bindings.is_empty() || !lift.is_finite() || *lift < 0.0 {
            return Err(evaluator_mismatch(recipe));
        }
        let base = pair(colours, name)?;
        let (surface_l, surface_c, surface_h) = linear_srgb_to_oklch(base.surface);
        let (foreground_l, _, _) = linear_srgb_to_oklch(base.foreground);
        let away_sign = if surface_l >= foreground_l { 1.0 } else { -1.0 };

        // This deliberately replaces ctk's unsafe default/destructive
        // `lighten(base, positive_delta)` path with the AA-clamped selected
        // state model. It is therefore not colour-identical to that CTK path;
        // Stage C must treat the difference as an accessibility correction,
        // not as a mechanical rename. `dimmed_on` is likewise not absorbed:
        // a ResolvedPair cannot carry CTK's second selected foreground.
        for step in (0..=STEPS).rev() {
            let amount = *lift * away_sign * f64::from(step) / f64::from(STEPS);
            let surface = oklch_to_linear_srgb(
                (surface_l + amount).clamp(0.0, 1.0),
                surface_c,
                surface_h,
                base.surface.alpha,
            )
            .map_err(str::to_owned)?;
            let candidate = resolved_pair(
                base.surface_name.clone(),
                surface,
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            )?;
            let extras_clear = extra.iter().all(|foreground| {
                let rendered = if foreground.opaque() {
                    *foreground
                } else {
                    composite(*foreground, candidate.rendered_surface)
                };
                contrast_ratio(rendered, candidate.rendered_surface) >= TEXT_CONTRAST
            });
            if candidate.contrast_ratio >= TEXT_CONTRAST
                && extras_clear
                && delivered_member_changed(RecipeMovement::Surface, base, &candidate)
            {
                return Ok(RecipeEvaluation {
                    pair: candidate,
                    warning: None,
                });
            }
        }
        Err("contrast_safe_state_pair has no AA candidate for every foreground".into())
    }

    fn pair<'a>(colours: &'a ResolvedColours, name: &str) -> Result<&'a ResolvedPair, String> {
        colours
            .pairs
            .get(name)
            .ok_or_else(|| format!("`{name}` is not a resolved semantic pair"))
    }

    fn evaluator_mismatch(recipe: &DerivationRecipe) -> String {
        format!("`{}` bindings do not match its evaluator", recipe.name)
    }

    fn resolved_pair(
        surface_name: String,
        surface: LinearRgba,
        foreground_name: String,
        foreground: LinearRgba,
        backdrop_name: Option<String>,
        backdrop: Option<LinearRgba>,
    ) -> Result<ResolvedPair, String> {
        if (!surface.opaque() || !foreground.opaque()) && backdrop.is_none() {
            return Err("a translucent derived text pair requires a backdrop".into());
        }
        let rendered_surface = backdrop.map_or(surface, |under| composite(surface, under));
        let rendered_foreground = if foreground.opaque() {
            foreground
        } else {
            composite(foreground, rendered_surface)
        };
        if !rendered_surface.opaque() || !rendered_foreground.opaque() {
            return Err("a derived pair must resolve to an opaque composite".into());
        }
        Ok(ResolvedPair {
            surface_name,
            surface,
            foreground_name,
            foreground,
            backdrop_name,
            backdrop,
            rendered_surface,
            rendered_foreground,
            contrast_ratio: contrast_ratio(rendered_foreground, rendered_surface),
            recipe: None,
        })
    }

    pub(crate) fn contrast_safe_lift(
        pair: &ResolvedPair,
        magnitude: f64,
    ) -> Result<ResolvedPair, String> {
        let (lightness, chroma, hue) = linear_srgb_to_oklch(pair.surface);
        let (foreground_lightness, _, _) = linear_srgb_to_oklch(pair.foreground);
        let away_sign = if lightness >= foreground_lightness {
            1.0
        } else {
            -1.0
        };
        let requested_lift = magnitude * away_sign;
        for step in (0..=STEPS).rev() {
            let amount = requested_lift * f64::from(step) / f64::from(STEPS);
            let Ok(surface) = oklch_to_linear_srgb(
                (lightness + amount).clamp(0.0, 1.0),
                chroma,
                hue,
                pair.surface.alpha,
            ) else {
                continue;
            };
            let rendered_surface = pair
                .backdrop
                .map_or(surface, |under| composite(surface, under));
            let rendered_foreground = if pair.foreground.opaque() {
                pair.foreground
            } else {
                composite(pair.foreground, rendered_surface)
            };
            let ratio = contrast_ratio(rendered_foreground, rendered_surface);
            let candidate = ResolvedPair {
                surface_name: pair.surface_name.clone(),
                surface,
                foreground_name: pair.foreground_name.clone(),
                foreground: pair.foreground,
                backdrop_name: pair.backdrop_name.clone(),
                backdrop: pair.backdrop,
                rendered_surface,
                rendered_foreground,
                contrast_ratio: ratio,
                recipe: pair.recipe.clone(),
            };
            if ratio >= TEXT_CONTRAST
                && delivered_member_changed(RecipeMovement::Surface, pair, &candidate)
            {
                return Ok(candidate);
            }
        }
        Err(format!(
            "pair `{}` has no AA-compliant contrast-safe lift amount that changes a delivered byte",
            pair.surface_name
        ))
    }

    pub(crate) fn composite(over: LinearRgba, under: LinearRgba) -> LinearRgba {
        let alpha = over.alpha + under.alpha * (1.0 - over.alpha);
        if alpha <= f64::EPSILON {
            return LinearRgba {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 0.0,
            };
        }
        let channel = |top: f64, bottom: f64| {
            (top * over.alpha + bottom * under.alpha * (1.0 - over.alpha)) / alpha
        };
        LinearRgba {
            red: channel(over.red, under.red),
            green: channel(over.green, under.green),
            blue: channel(over.blue, under.blue),
            alpha,
        }
    }

    pub(crate) fn oklch_to_linear_srgb(
        lightness: f64,
        chroma: f64,
        hue: f64,
        alpha: f64,
    ) -> Result<LinearRgba, &'static str> {
        if !lightness.is_finite() || !chroma.is_finite() || !hue.is_finite() || !alpha.is_finite() {
            return Err("OKLCH channels must be finite");
        }
        if !(0.0..=1.0).contains(&lightness) || chroma < 0.0 || !(0.0..=1.0).contains(&alpha) {
            return Err("OKLCH L and alpha must be in 0..=1 and chroma must be non-negative");
        }

        let rgb = gamut_map_oklch(lightness, chroma, hue, 24);
        Ok(LinearRgba {
            red: rgb[0].clamp(0.0, 1.0),
            green: rgb[1].clamp(0.0, 1.0),
            blue: rgb[2].clamp(0.0, 1.0),
            alpha,
        })
    }

    fn gamut_map_oklch(lightness: f64, chroma: f64, hue: f64, iterations: usize) -> [f64; 3] {
        let requested = oklch_at_chroma(lightness, chroma, hue);
        if in_gamut(requested) {
            return requested;
        }

        let mut low = 0.0;
        let mut high = chroma;
        // Zero chroma is the always-available in-gamut floor. Retaining it
        // before the search makes the iteration count a quality budget only:
        // exhaustion can reduce preserved chroma, never return an invalid
        // candidate for the caller to disguise by channel clamping.
        let mut retained = oklch_at_chroma(lightness, low, hue);
        debug_assert!(in_gamut(retained));
        for _ in 0..iterations {
            let middle = (low + high) * 0.5;
            let candidate = oklch_at_chroma(lightness, middle, hue);
            if in_gamut(candidate) {
                low = middle;
                retained = candidate;
            } else {
                high = middle;
            }
        }
        retained
    }

    fn oklch_at_chroma(lightness: f64, chroma: f64, hue: f64) -> [f64; 3] {
        let radians = hue.rem_euclid(360.0).to_radians();
        let a = chroma * radians.cos();
        let b = chroma * radians.sin();
        let l = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
        let m = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
        let s = lightness - 0.089_484_177_5 * a - 1.291_485_548 * b;
        let l = l * l * l;
        let m = m * m * m;
        let s = s * s * s;
        [
            4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
            -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
            -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s,
        ]
    }

    pub(crate) fn linear_srgb_to_oklch(value: LinearRgba) -> (f64, f64, f64) {
        let l = (0.412_221_470_8 * value.red
            + 0.536_332_536_3 * value.green
            + 0.051_445_992_9 * value.blue)
            .cbrt();
        let m = (0.211_903_498_2 * value.red
            + 0.680_699_545_1 * value.green
            + 0.107_396_956_6 * value.blue)
            .cbrt();
        let s = (0.088_302_461_9 * value.red
            + 0.281_718_837_6 * value.green
            + 0.629_978_700_5 * value.blue)
            .cbrt();
        let lightness = 0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s;
        let a = 1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s;
        let b = 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s;
        let chroma = a.hypot(b);
        let hue = b.atan2(a).to_degrees().rem_euclid(360.0);
        (lightness, chroma, hue)
    }

    fn in_gamut(rgb: [f64; 3]) -> bool {
        rgb.into_iter()
            .all(|channel| (-1e-7..=1.0 + 1e-7).contains(&channel))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::BTreeMap;

        fn colour(lightness: f64, alpha: f64) -> LinearRgba {
            oklch_to_linear_srgb(lightness, 0.0, 0.0, alpha).unwrap()
        }

        fn linear_grey(luminance: f64, alpha: f64) -> LinearRgba {
            LinearRgba {
                red: luminance,
                green: luminance,
                blue: luminance,
                alpha,
            }
        }

        fn chromatic_colour(lightness: f64, chroma: f64, hue: f64, alpha: f64) -> LinearRgba {
            oklch_to_linear_srgb(lightness, chroma, hue, alpha).unwrap()
        }

        #[test]
        fn gamut_search_retains_an_in_gamut_floor_when_the_budget_is_exhausted() {
            let lightness = 1.0;
            let chroma = 0.322_490_964_775_164_37;
            let hue = 0.0;
            assert!(
                !in_gamut(oklch_at_chroma(lightness, chroma, hue)),
                "the fixture must exercise gamut reduction"
            );

            for iterations in [0, 1, 8, 24] {
                let retained = gamut_map_oklch(lightness, chroma, hue, iterations);
                assert!(
                    in_gamut(retained),
                    "gamut search returned an invalid retained candidate with budget {iterations}: {retained:?}"
                );
            }
        }

        fn opaque_pair(surface_l: f64, foreground_l: f64) -> ResolvedPair {
            resolved_pair(
                "surface".into(),
                colour(surface_l, 1.0),
                "foreground".into(),
                colour(foreground_l, 1.0),
                None,
                None,
            )
            .unwrap()
        }

        fn pair_recipe(name: &'static str, pair: &str, lift: f64) -> DerivationRecipe {
            DerivationRecipe {
                name,
                bindings: vec![
                    RecipeBinding::Pair { name: pair.into() },
                    RecipeBinding::Ratio {
                        name: "lift".into(),
                        value: lift,
                    },
                ],
                implicit_bindings: Vec::new(),
                substitutable_slot: Some(0),
                movement: crate::recipe::REGISTRY
                    .iter()
                    .find(|signature| signature.name == name)
                    .unwrap()
                    .movement,
                substitution_domain_constraints: &[],
                output: crate::recipe::RecipeOutput::Pair,
                text_contrast_postcondition: true,
                non_text_contrast_postcondition: false,
                opaque_input_precondition: None,
                opaque_output_invariant: false,
                substitution_policy: None,
            }
        }

        fn disabled_recipe(pair: &str, lift: f64, chroma_reduction: f64) -> DerivationRecipe {
            let mut recipe = pair_recipe("disabled_pair", pair, lift);
            recipe.bindings.push(RecipeBinding::Ratio {
                name: "chroma_reduction".into(),
                value: chroma_reduction,
            });
            recipe
        }

        #[test]
        fn away_and_toward_have_opposite_order_for_both_pair_polarities() {
            for (surface_l, foreground_l) in [(0.80, 0.10), (0.20, 0.90)] {
                let base = opaque_pair(surface_l, foreground_l);
                let away = contrast_safe_lift(&base, 0.03).unwrap();
                let toward = surface_toward_foreground("base", &base, 0.03).unwrap();
                let away_l = linear_srgb_to_oklch(away.surface).0;
                let toward_l = linear_srgb_to_oklch(toward.surface).0;
                if surface_l > foreground_l {
                    assert!(away_l > surface_l);
                    assert!(toward_l < surface_l);
                } else {
                    assert!(away_l < surface_l);
                    assert!(toward_l > surface_l);
                }
                assert!(away.contrast_ratio >= base.contrast_ratio);
                assert!(toward.contrast_ratio < base.contrast_ratio);
                assert!(toward.contrast_ratio >= TEXT_CONTRAST);
            }
        }

        #[test]
        fn away_and_toward_do_not_converge_at_the_light_endpoint() {
            let base = opaque_pair(0.98, 0.20);
            let away = contrast_safe_lift(&base, 0.06).unwrap();
            let toward = surface_toward_foreground("base", &base, 0.06).unwrap();
            assert!((linear_srgb_to_oklch(away.surface).0 - 1.0).abs() < 1e-7);
            assert!(linear_srgb_to_oklch(toward.surface).0 < 0.98);
            assert_ne!(away.surface.to_srgba8(), toward.surface.to_srgba8());
        }

        #[test]
        fn all_five_moving_derivations_reject_zero_delivered_progress() {
            // This pins the generic non-zero delivered-movement requirement
            // across all five walkers. The wrong-member probes below cover
            // the distinct requirement that movement belongs to the declared
            // member.
            let barely_visible = resolved_pair(
                "barely-visible.surface".into(),
                colour(0.95, 0.01),
                "black".into(),
                LinearRgba::BLACK,
                Some("white".into()),
                Some(LinearRgba::WHITE),
            )
            .unwrap();
            assert!(barely_visible.contrast_ratio > 20.0);
            for error in [
                surface_toward_foreground("barely-visible", &barely_visible, 0.001).unwrap_err(),
                disabled_surface_toward_foreground("barely-visible", &barely_visible, 0.001, 0.0)
                    .unwrap_err(),
            ] {
                assert!(error.contains("pair `barely-visible`"), "{error}");
                assert!(error.contains("0.001"), "{error}");
            }

            let transparent_floor = resolved_pair(
                "transparent".into(),
                linear_grey(0.0, 0.0),
                "floor.foreground".into(),
                linear_grey(0.183_333_2, 1.0),
                Some("white".into()),
                Some(LinearRgba::WHITE),
            )
            .unwrap();
            assert!(transparent_floor.contrast_ratio > 4.5);
            let error = disabled_foreground_toward_rendered_surface(
                "transparent-floor",
                &transparent_floor,
                0.001,
                0.0,
            )
            .unwrap_err();
            assert!(error.contains("pair `transparent-floor`"), "{error}");
            assert!(error.contains("0.001"), "{error}");

            let error = contrast_safe_lift(&transparent_floor, 0.03).unwrap_err();
            assert!(error.contains("pair `transparent`"), "{error}");
            assert!(error.contains("delivered byte"), "{error}");

            let colours = ResolvedColours {
                pairs: BTreeMap::from([("transparent".into(), transparent_floor)]),
                ..Default::default()
            };
            let state_recipe = DerivationRecipe {
                name: "contrast_safe_state_pair",
                bindings: vec![
                    RecipeBinding::Pair {
                        name: "transparent".into(),
                    },
                    RecipeBinding::ColourList {
                        names: vec!["black".into()],
                        values: vec![LinearRgba::BLACK],
                    },
                    RecipeBinding::Ratio {
                        name: "lift".into(),
                        value: 0.03,
                    },
                ],
                implicit_bindings: Vec::new(),
                substitutable_slot: Some(0),
                movement: RecipeMovement::Surface,
                substitution_domain_constraints: &[],
                output: crate::recipe::RecipeOutput::Pair,
                text_contrast_postcondition: true,
                non_text_contrast_postcondition: false,
                opaque_input_precondition: None,
                opaque_output_invariant: false,
                substitution_policy: None,
            };
            let error = evaluate_pair_recipe(&state_recipe, &colours).unwrap_err();
            assert!(error.contains("no AA candidate"), "{error}");
        }

        #[test]
        fn exact_translucent_lift_probe_rejects_wrong_member_progress() {
            let base = resolved_pair(
                "probe.surface".into(),
                colour(0.795_224_918_5, 1.0 / 255.0),
                "probe.foreground".into(),
                LinearRgba {
                    alpha: 137.0 / 255.0,
                    ..LinearRgba::WHITE
                },
                Some("probe.backdrop".into()),
                Some(linear_grey(0.036_889, 1.0)),
            )
            .unwrap();
            assert_eq!(base.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(base.rendered_foreground.to_srgba8(), [197, 197, 197, 255]);
            let error = contrast_safe_lift(&base, 0.0001).unwrap_err();
            assert!(error.contains("no AA-compliant"), "{error}");
            assert!(error.contains("delivered byte"), "{error}");
        }

        #[test]
        fn surface_toward_foreground_rejects_foreground_only_delivered_progress() {
            let base = resolved_pair(
                "probe.surface".into(),
                colour(0.795_092_8, 1.0 / 255.0),
                "probe.foreground".into(),
                LinearRgba {
                    alpha: 137.0 / 255.0,
                    ..LinearRgba::WHITE
                },
                Some("probe.backdrop".into()),
                Some(linear_grey(0.036_889, 1.0)),
            )
            .unwrap();
            assert_eq!(base.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(base.rendered_foreground.to_srgba8(), [196, 196, 196, 255]);

            let full_step = resolved_pair(
                base.surface_name.clone(),
                colour(0.795_192_8, 1.0 / 255.0),
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            )
            .unwrap();
            assert_eq!(full_step.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(
                full_step.rendered_foreground.to_srgba8(),
                [197, 197, 197, 255]
            );
            assert!(full_step.contrast_ratio >= TEXT_CONTRAST);

            let error = surface_toward_foreground("probe", &base, 0.0001).unwrap_err();
            assert!(error.contains("pair `probe`"), "{error}");
            assert!(error.contains("0.0001"), "{error}");
        }

        #[test]
        fn disabled_surface_toward_foreground_rejects_foreground_only_delivered_progress() {
            let base = resolved_pair(
                "probe.surface".into(),
                colour(0.795_092_8, 1.0 / 255.0),
                "probe.foreground".into(),
                LinearRgba {
                    alpha: 137.0 / 255.0,
                    ..LinearRgba::WHITE
                },
                Some("probe.backdrop".into()),
                Some(linear_grey(0.036_889, 1.0)),
            )
            .unwrap();
            assert_eq!(base.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(base.rendered_foreground.to_srgba8(), [196, 196, 196, 255]);

            let full_step = resolved_pair(
                base.surface_name.clone(),
                colour(0.795_192_8, 1.0 / 255.0),
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            )
            .unwrap();
            assert_eq!(full_step.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(
                full_step.rendered_foreground.to_srgba8(),
                [197, 197, 197, 255]
            );
            assert!(full_step.contrast_ratio >= TEXT_CONTRAST);

            let error =
                disabled_surface_toward_foreground("probe", &base, 0.0001, 0.0).unwrap_err();
            assert!(error.contains("pair `probe`"), "{error}");
            assert!(error.contains("0.0001"), "{error}");
        }

        #[test]
        fn contrast_safe_state_pair_rejects_foreground_only_delivered_progress() {
            let base = resolved_pair(
                "probe.surface".into(),
                colour(0.795_192_8, 1.0 / 255.0),
                "probe.foreground".into(),
                LinearRgba {
                    alpha: 137.0 / 255.0,
                    ..LinearRgba::WHITE
                },
                Some("probe.backdrop".into()),
                Some(linear_grey(0.036_889, 1.0)),
            )
            .unwrap();
            assert_eq!(base.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(base.rendered_foreground.to_srgba8(), [197, 197, 197, 255]);

            let full_step = resolved_pair(
                base.surface_name.clone(),
                colour(0.795_092_8, 1.0 / 255.0),
                base.foreground_name.clone(),
                base.foreground,
                base.backdrop_name.clone(),
                base.backdrop,
            )
            .unwrap();
            assert_eq!(full_step.rendered_surface.to_srgba8(), [55, 55, 55, 255]);
            assert_eq!(
                full_step.rendered_foreground.to_srgba8(),
                [196, 196, 196, 255]
            );
            assert!(full_step.contrast_ratio >= TEXT_CONTRAST);

            let colours = ResolvedColours {
                pairs: BTreeMap::from([("probe".into(), base)]),
                ..Default::default()
            };
            let recipe = DerivationRecipe {
                name: "contrast_safe_state_pair",
                bindings: vec![
                    RecipeBinding::Pair {
                        name: "probe".into(),
                    },
                    RecipeBinding::ColourList {
                        names: Vec::new(),
                        values: Vec::new(),
                    },
                    RecipeBinding::Ratio {
                        name: "lift".into(),
                        value: 0.0001,
                    },
                ],
                implicit_bindings: Vec::new(),
                substitutable_slot: Some(0),
                movement: RecipeMovement::Surface,
                substitution_domain_constraints: &[],
                output: crate::recipe::RecipeOutput::Pair,
                text_contrast_postcondition: true,
                non_text_contrast_postcondition: false,
                opaque_input_precondition: None,
                opaque_output_invariant: false,
                substitution_policy: None,
            };
            let error = evaluate_contrast_safe_state_pair(&recipe, &colours).unwrap_err();
            assert!(error.contains("no AA candidate"), "{error}");
        }

        #[test]
        fn evaluator_boundary_rejects_a_declared_member_left_unchanged() {
            let base = opaque_pair(0.80, 0.10);
            let colours = ResolvedColours {
                pairs: BTreeMap::from([("base".into(), base)]),
                ..Default::default()
            };
            let mut recipe = pair_recipe("contrast_safe_lift", "base", 0.03);
            // The surface evaluator succeeds locally, while this deliberately
            // altered declaration asks the boundary to verify the unchanged
            // opaque foreground. This isolates the generic backstop.
            recipe.movement = RecipeMovement::Foreground;
            let error = evaluate_pair_recipe(&recipe, &colours).unwrap_err();
            assert!(error.contains("recipe `contrast_safe_lift`"), "{error}");
            assert!(error.contains("delivered foreground byte"), "{error}");
            assert!(error.contains("base pair `base`"), "{error}");
        }

        #[test]
        fn authored_domain_rejects_transparent_surface_for_lift_and_state_recipes() {
            let transparent = resolved_pair(
                "transparent.surface".into(),
                linear_grey(0.0, 0.0),
                "black".into(),
                LinearRgba::BLACK,
                Some("white".into()),
                Some(LinearRgba::WHITE),
            )
            .unwrap();
            let colours = ResolvedColours {
                pairs: BTreeMap::from([("transparent".into(), transparent)]),
                ..Default::default()
            };

            for mut recipe in [
                pair_recipe("contrast_safe_lift", "transparent", 0.03),
                DerivationRecipe {
                    name: "contrast_safe_state_pair",
                    bindings: vec![
                        RecipeBinding::Pair {
                            name: "transparent".into(),
                        },
                        RecipeBinding::ColourList {
                            names: vec!["black".into()],
                            values: vec![LinearRgba::BLACK],
                        },
                        RecipeBinding::Ratio {
                            name: "lift".into(),
                            value: 0.03,
                        },
                    ],
                    implicit_bindings: Vec::new(),
                    substitutable_slot: Some(0),
                    movement: RecipeMovement::Surface,
                    substitution_domain_constraints: &[],
                    output: crate::recipe::RecipeOutput::Pair,
                    text_contrast_postcondition: true,
                    non_text_contrast_postcondition: false,
                    opaque_input_precondition: None,
                    opaque_output_invariant: false,
                    substitution_policy: None,
                },
            ] {
                recipe.substitution_domain_constraints = crate::recipe::REGISTRY
                    .iter()
                    .find(|signature| signature.name == recipe.name)
                    .unwrap()
                    .substitution_domain_constraints;
                let error = evaluate_pair_recipe(&recipe, &colours).unwrap_err();
                assert!(error.contains(recipe.name), "{error}");
                assert!(error.contains("pair `transparent`"), "{error}");
                assert!(error.contains("outside its domain"), "{error}");
                assert!(error.contains("non-transparent surface"), "{error}");
            }
        }

        #[test]
        fn disabled_pair_routes_every_positive_alpha_through_the_surface_walker() {
            let base = resolved_pair(
                "almost-transparent.surface".into(),
                colour(0.95, f64::EPSILON / 2.0),
                "black".into(),
                LinearRgba::BLACK,
                Some("white".into()),
                Some(LinearRgba::WHITE),
            )
            .unwrap();
            let colours = ResolvedColours {
                pairs: BTreeMap::from([("base".into(), base)]),
                ..Default::default()
            };
            let error =
                evaluate_pair_recipe(&disabled_recipe("base", 0.03, 0.5), &colours).unwrap_err();
            assert!(error.contains("disabled surface"), "{error}");
        }

        #[test]
        fn toward_walks_never_cross_the_target_lightness() {
            let target = linear_grey(0.18, 1.0);
            let visible = resolved_pair(
                "black".into(),
                LinearRgba::BLACK,
                "target".into(),
                target,
                None,
                None,
            )
            .unwrap();
            assert!(visible.contrast_ratio > TEXT_CONTRAST);
            let target_l = linear_srgb_to_oklch(target).0;
            for output in [
                surface_toward_foreground("visible", &visible, 1.0).unwrap(),
                disabled_surface_toward_foreground("visible", &visible, 1.0, 0.0).unwrap(),
            ] {
                let output_l = linear_srgb_to_oklch(output.surface).0;
                assert!(output_l > 0.0);
                assert!(output_l <= target_l + 1e-12);
            }

            let transparent = resolved_pair(
                "transparent".into(),
                linear_grey(0.0, 0.0),
                "black".into(),
                LinearRgba::BLACK,
                Some("target".into()),
                Some(target),
            )
            .unwrap();
            let output =
                disabled_foreground_toward_rendered_surface("transparent", &transparent, 1.0, 0.0)
                    .unwrap();
            let output_l = linear_srgb_to_oklch(output.foreground).0;
            assert!(output_l > 0.0);
            assert!(output_l <= target_l + 1e-12);
        }

        #[test]
        fn disabled_transparent_pair_holds_surface_and_moves_foreground() {
            let surface = colour(0.0, 0.0);
            let backdrop = colour(0.95, 1.0);
            let foreground = chromatic_colour(0.20, 0.08, 30.0, 1.0);
            let base = resolved_pair(
                "transparent".into(),
                surface,
                "foreground".into(),
                foreground,
                Some("backdrop".into()),
                Some(backdrop),
            )
            .unwrap();
            let output =
                disabled_foreground_toward_rendered_surface("muted", &base, 0.03, 0.5).unwrap();
            assert_eq!(output.surface.to_srgba8(), base.surface.to_srgba8());
            assert_eq!(
                output.rendered_surface.to_srgba8(),
                base.rendered_surface.to_srgba8()
            );
            assert_ne!(
                output.rendered_foreground.to_srgba8(),
                base.rendered_foreground.to_srgba8()
            );
            assert!(
                linear_srgb_to_oklch(output.foreground).1 < linear_srgb_to_oklch(base.foreground).1
            );
            assert!(output.contrast_ratio < base.contrast_ratio);
            assert!(output.contrast_ratio >= TEXT_CONTRAST);
        }

        #[test]
        fn disabled_visible_pair_moves_lightness_and_chroma_toward_neutral() {
            let base = resolved_pair(
                "surface".into(),
                chromatic_colour(0.75, 0.12, 30.0, 1.0),
                "foreground".into(),
                colour(0.10, 1.0),
                None,
                None,
            )
            .unwrap();
            let colours = ResolvedColours {
                pairs: BTreeMap::from([("base".into(), base.clone())]),
                ..Default::default()
            };
            let output = evaluate_pair_recipe(&disabled_recipe("base", 0.04, 0.5), &colours)
                .unwrap()
                .pair;
            let (base_l, base_c, _) = linear_srgb_to_oklch(base.surface);
            let (output_l, output_c, _) = linear_srgb_to_oklch(output.surface);
            assert!(output_l < base_l);
            assert!((output_c / base_c - 0.5).abs() < 1e-6);
            assert_eq!(output.foreground.to_srgba8(), base.foreground.to_srgba8());
            assert!(output.contrast_ratio < base.contrast_ratio);
            assert!(output.contrast_ratio >= TEXT_CONTRAST);
        }

        #[test]
        fn disabled_pair_rejects_chroma_reduction_outside_the_unit_interval() {
            let base = opaque_pair(0.75, 0.10);
            let colours = ResolvedColours {
                pairs: BTreeMap::from([("base".into(), base)]),
                ..Default::default()
            };
            for chroma_reduction in [-0.01, 1.01] {
                assert!(
                    evaluate_pair_recipe(
                        &disabled_recipe("base", 0.06, chroma_reduction),
                        &colours
                    )
                    .is_err()
                );
            }
        }

        #[test]
        fn toward_walks_clamp_before_crossing_the_aa_floor() {
            let visible = opaque_pair(0.60, 0.0);
            assert!(contrast_ratio(colour(0.50, 1.0), visible.foreground) < TEXT_CONTRAST);
            let visible_output = surface_toward_foreground("visible", &visible, 0.10).unwrap();
            let visible_l = linear_srgb_to_oklch(visible_output.surface).0;
            assert!(visible_l > 0.50 && visible_l < 0.60);
            assert!(visible_output.contrast_ratio >= TEXT_CONTRAST);

            let transparent = resolved_pair(
                "transparent".into(),
                colour(0.0, 0.0),
                "foreground".into(),
                colour(0.45, 1.0),
                Some("backdrop".into()),
                Some(colour(0.95, 1.0)),
            )
            .unwrap();
            assert!(
                contrast_ratio(colour(0.60, 1.0), transparent.rendered_surface) < TEXT_CONTRAST
            );
            let transparent_output =
                disabled_foreground_toward_rendered_surface("transparent", &transparent, 0.15, 0.5)
                    .unwrap();
            let foreground_l = linear_srgb_to_oklch(transparent_output.foreground).0;
            assert!(foreground_l > 0.45 && foreground_l < 0.60);
            assert!(transparent_output.contrast_ratio >= TEXT_CONTRAST);
        }

        fn focus_recipe(seed_name: &str, seed: LinearRgba, pair_name: &str) -> DerivationRecipe {
            let signature = crate::recipe::REGISTRY
                .iter()
                .find(|signature| signature.name == "focus_ring")
                .unwrap();
            DerivationRecipe {
                name: signature.name,
                bindings: vec![
                    RecipeBinding::Colour {
                        name: seed_name.into(),
                        value: seed,
                    },
                    RecipeBinding::Pair {
                        name: pair_name.into(),
                    },
                ],
                implicit_bindings: Vec::new(),
                substitutable_slot: signature.substitutable_slot,
                movement: signature.movement,
                substitution_domain_constraints: signature.substitution_domain_constraints,
                output: signature.output,
                text_contrast_postcondition: signature.text_contrast_postcondition,
                non_text_contrast_postcondition: signature.non_text_contrast_postcondition,
                opaque_input_precondition: signature.opaque_input_precondition,
                opaque_output_invariant: signature.opaque_output_invariant,
                substitution_policy: None,
            }
        }

        fn focus_surface(name: &str, luminance: f64) -> ResolvedPair {
            let surface = LinearRgba {
                red: luminance,
                green: luminance,
                blue: luminance,
                alpha: 1.0,
            };
            ResolvedPair {
                surface_name: name.into(),
                surface,
                foreground_name: "white".into(),
                foreground: LinearRgba::WHITE,
                backdrop_name: None,
                backdrop: None,
                rendered_surface: surface,
                rendered_foreground: LinearRgba::WHITE,
                contrast_ratio: contrast_ratio(LinearRgba::WHITE, surface),
                recipe: None,
            }
        }

        #[test]
        fn focus_ring_terminal_endpoint_records_ordinal_and_actual_distance_separately() {
            let candidate =
                walk_focus_ring_direction(0.4237, 0.0, 0.0, WalkDirection::Up, |_, lightness| {
                    lightness.to_bits() == 1.0_f64.to_bits()
                })
                .unwrap()
                .expect("the terminal white endpoint must be evaluated");
            assert_eq!(candidate.provenance.step_index, 577);
            assert!((candidate.provenance.delta_l - 0.5763).abs() < 1e-12);
            assert_eq!(
                candidate.value,
                oklch_to_linear_srgb(1.0, 0.0, 0.0, 1.0).unwrap()
            );
        }

        #[test]
        fn focus_ring_zero_step_records_zero_actual_distance() {
            let seed = colour(0.8, 1.0);
            let recipe = focus_recipe("ring", seed, "black");
            let evaluation =
                evaluate_non_text_recipe_against(&recipe, &focus_surface("black", 0.0)).unwrap();
            assert_eq!(evaluation.provenance.step_index, 0);
            assert_eq!(evaluation.provenance.delta_l, 0.0);
            assert!(evaluation.warning.is_none());
        }

        #[test]
        fn focus_ring_warning_threshold_is_silent_at_300_and_fires_at_301() {
            let seed = colour(0.4, 1.0);
            let recipe = focus_recipe("ring", seed, "surface");
            let surface_for_step = |step: u32| {
                let seed_l = linear_srgb_to_oklch(seed).0;
                let candidate_l = seed_l + f64::from(step) / 1000.0;
                let previous_l = seed_l + f64::from(step - 1) / 1000.0;
                let candidate = colour(candidate_l, 1.0);
                let previous = colour(previous_l, 1.0);
                let boundary = (candidate.red + 0.05) / 3.0 - 0.05;
                let previous_boundary = (previous.red + 0.05) / 3.0 - 0.05;
                focus_surface("surface", (boundary + previous_boundary) * 0.5)
            };
            let at_300 = evaluate_non_text_recipe_against(&recipe, &surface_for_step(300)).unwrap();
            let at_301 = evaluate_non_text_recipe_against(&recipe, &surface_for_step(301)).unwrap();
            assert_eq!(at_300.provenance.step_index, 300);
            assert!(at_300.warning.is_none());
            assert_eq!(at_301.provenance.step_index, 301);
            assert_eq!(at_301.warning.unwrap().code, "ring-walk-distance");
        }

        #[test]
        fn focus_ring_equal_step_tie_chooses_the_lighter_candidate() {
            let surface_luminance = 0.2_f64;
            let dark_limit = ((surface_luminance + 0.05) / 3.0 - 0.05).cbrt();
            let light_limit = (3.0 * (surface_luminance + 0.05) - 0.05).cbrt();
            let seed_l = (dark_limit + light_limit) * 0.5;
            let surface = focus_surface("surface", surface_luminance);
            let down =
                focus_ring_direction_first_pass(seed_l, 0.0, 0.0, &surface, WalkDirection::Down)
                    .unwrap()
                    .unwrap();
            let up = focus_ring_direction_first_pass(seed_l, 0.0, 0.0, &surface, WalkDirection::Up)
                .unwrap()
                .unwrap();
            assert_eq!(down.provenance.step_index, up.provenance.step_index);
            let recipe = focus_recipe("ring", colour(seed_l, 1.0), "surface");
            let chosen = evaluate_non_text_recipe_against(&recipe, &surface).unwrap();
            assert_eq!(chosen.value.to_srgba8(), up.value.to_srgba8());
            assert_ne!(chosen.value.to_srgba8(), down.value.to_srgba8());
        }

        #[test]
        fn focus_ring_postcondition_and_output_opacity_check_actual_output() {
            let recipe = focus_recipe("ring", LinearRgba::WHITE, "surface");
            let surface = focus_surface("surface", 1.0);
            let check = |value: LinearRgba, expected: &str| {
                let error = evaluate_non_text_recipe_against_with(&recipe, &surface, |_, _| {
                    Ok(NonTextRecipeEvaluation {
                        value,
                        provenance: FocusRingProvenance {
                            seed_name: "ring".into(),
                            step_index: 0,
                            delta_l: 0.0,
                        },
                        warning: None,
                    })
                })
                .expect_err("production NonText postcheck accepted invalid evaluator output");
                assert!(error.contains(expected), "{error}");
            };

            check(LinearRgba::WHITE, "non-text contrast");
            check(
                LinearRgba {
                    alpha: 0.5,
                    ..LinearRgba::BLACK
                },
                "opaque output is invariant",
            );
        }

        #[test]
        fn focus_ring_rejects_a_translucent_authored_seed() {
            let seed = LinearRgba {
                alpha: 0.5,
                ..LinearRgba::WHITE
            };
            let error = evaluate_non_text_recipe_against(
                &focus_recipe("ring", seed, "surface"),
                &focus_surface("surface", 0.0),
            )
            .unwrap_err();
            assert!(error.contains("opaque seed colour `ring`"));
        }

        #[test]
        fn focus_ring_opacity_precondition_is_exact() {
            let seed = LinearRgba {
                alpha: f64::from_bits(1.0_f64.to_bits() - 1),
                ..LinearRgba::WHITE
            };
            let error = evaluate_non_text_recipe_against(
                &focus_recipe("ring", seed, "surface"),
                &focus_surface("surface", 0.0),
            )
            .unwrap_err();
            assert!(error.contains("opaque seed colour `ring`"));
        }

        /// `opaque()` tests `alpha == 1.0` exactly, while `rendered_surface`
        /// and `rendered_foreground` carry a composited alpha. Compositing over
        /// an opaque backdrop must not be falsely rejected: with
        /// `under.alpha == 1.0`, the current expression returns exactly 1.0 for
        /// every finite authored alpha in [0, 1].
        ///
        /// This pins only the no-false-rejection guarantee. It does not claim
        /// that every translucent-under composition remains distinguishable
        /// from one; the accepted rounding residual is pinned separately.
        #[test]
        fn compositing_over_an_opaque_backdrop_is_exactly_opaque() {
            let opaque_under = LinearRgba {
                alpha: 1.0,
                ..LinearRgba::BLACK
            };
            for step in 0..=1000 {
                let a = f64::from(step) / 1000.0;
                let over = LinearRgba {
                    alpha: a,
                    ..LinearRgba::WHITE
                };
                let rendered = composite(over, opaque_under);
                assert_eq!(
                    rendered.alpha, 1.0,
                    "compositing alpha {a} over an opaque backdrop must be exactly opaque"
                );
                assert!(rendered.opaque());
            }
        }

        /// Binary64 rounds a true result to 1.0 when it lies no more than
        /// 2^-54 below one. For equal input alphas `a`, the true composited
        /// deficit is `(1-a)^2`, so inputs up to 2^-27 below one can enter that
        /// rounding interval. The omitted deeper-backdrop contribution is then
        /// at most 2^-54 per linear channel; even a conservative WCAG-ratio
        /// sensitivity bound of 440 keeps the resulting ratio error below
        /// 3e-14, immaterial to SPEC 19's 3:1 and 4.5:1 measurements.
        #[test]
        fn near_opaque_translucent_layers_can_round_to_exactly_opaque() {
            let alpha = 0.999_999_999;
            let under = LinearRgba {
                alpha,
                ..LinearRgba::BLACK
            };
            let over = LinearRgba {
                alpha,
                ..LinearRgba::WHITE
            };
            let rendered = composite(over, under);
            let true_deficit = (1.0 - alpha) * (1.0 - alpha);
            assert!(true_deficit > 0.0);
            assert!(true_deficit < 2_f64.powi(-54));
            assert_eq!(rendered.alpha, 1.0);
            assert!(rendered.opaque());
        }

        #[test]
        fn ordinary_translucent_layers_remain_non_opaque() {
            let rendered = composite(
                LinearRgba {
                    alpha: 0.25,
                    ..LinearRgba::WHITE
                },
                LinearRgba {
                    alpha: 0.5,
                    ..LinearRgba::BLACK
                },
            );
            assert!(rendered.alpha < 1.0);
            assert!(!rendered.opaque());
        }
    }
}

fn linear_to_srgb(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

fn quantise(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}
