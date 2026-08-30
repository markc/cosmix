use std::collections::{BTreeMap, BTreeSet};

use crate::DesignContext;
use crate::colour_model::ResolvedColours;
use crate::colour_model::derivation::{
    NonTextRecipeEvaluation, evaluate_non_text_recipe_against, evaluate_pair_recipe,
    validate_evaluator_signature, verify_non_text_postcondition, verify_text_postcondition,
};
use crate::diagnostic::DesignDiagnostic;
use crate::recipe::{
    DerivationRecipe, PairRefDecision, PairRefExclusion, RecipeBinding, RecipeImplicitBinding,
    RecipeImplicitInput, RecipeMovement, RecipeOutput, RecipeParam, RecipeSignature,
};
use crate::source::RecipeArgumentSource;

pub(crate) fn validate_recipe_registry(
    registry: &[RecipeSignature],
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> bool {
    let mut valid = true;
    let mut names = BTreeSet::new();
    for signature in registry {
        let row_path = format!("{path}.{}", signature.name);
        if !names.insert(signature.name) {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                &row_path,
                format!("duplicate registry row `{}`", signature.name),
            ));
            valid = false;
        }
        valid &= validate_signature(signature, &row_path, errors);
        if let Err(message) = validate_evaluator_signature(signature) {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                &row_path,
                message,
            ));
            valid = false;
        }
    }
    valid
}

pub(crate) fn compile_pair_substitution_policy(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<DerivationRecipe> {
    let Some(slot) = recipe.substitutable_slot else {
        return Some(recipe.clone());
    };
    let Some(RecipeBinding::Pair {
        name: authored_pair,
    }) = recipe.bindings.get(slot)
    else {
        errors.push(DesignDiagnostic::error(
            "invalid-derivation-signature",
            path,
            format!(
                "`{}` retained an invalid substitutable binding at authored argument {slot}",
                recipe.name
            ),
        ));
        return None;
    };

    let constraint = recipe
        .substitution_domain_constraints
        .iter()
        .find(|constraint| constraint.param_index == slot);
    let decisions = colours
        .pairs
        .iter()
        .map(|(pair_name, pair)| {
            let decision = match constraint {
                Some(constraint) if !constraint.domain.admits_surface_alpha(pair.surface.alpha) => {
                    PairRefDecision::Excluded(PairRefExclusion::OutsideRecipeDomain {
                        required: constraint.domain,
                    })
                }
                _ => PairRefDecision::Admitted,
            };
            (pair_name.clone(), decision)
        })
        .collect::<BTreeMap<_, _>>();
    if !decisions
        .values()
        .any(|decision| *decision == PairRefDecision::Admitted)
    {
        errors.push(DesignDiagnostic::error(
            "empty-override-product-domain",
            path,
            format!(
                "recipe `{}` admits no pair from the resolved pair dictionary at argument {slot}",
                recipe.name
            ),
        ));
        return None;
    }
    if decisions.get(authored_pair) != Some(&PairRefDecision::Admitted) {
        errors.push(DesignDiagnostic::error(
            "authored-pair-outside-override-domain",
            path,
            format!(
                "recipe `{}` authored argument {slot} pair `{authored_pair}` is not admitted by its compiled substitution policy",
                recipe.name
            ),
        ));
        return None;
    }
    let expected_pair_names = colours.pairs.keys().cloned().collect::<BTreeSet<_>>();
    match recipe
        .clone()
        .with_substitution_policy(slot, decisions, &expected_pair_names)
    {
        Ok(recipe) => Some(recipe),
        Err(message) => {
            errors.push(DesignDiagnostic::error(
                "invalid-compiled-substitution-policy",
                path,
                message,
            ));
            None
        }
    }
}

pub(crate) fn validate_override_product(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    cell_pair_derivation: Option<&DerivationRecipe>,
    path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
) {
    validate_override_product_with_non_text_evaluator(
        recipe,
        colours,
        cell_pair_derivation,
        path,
        warnings,
        errors,
        evaluate_non_text_recipe_against,
    );
}

#[allow(clippy::too_many_arguments)]
fn validate_override_product_with_non_text_evaluator<F>(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    cell_pair_derivation: Option<&DerivationRecipe>,
    path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    mut evaluate_non_text: F,
) where
    F: FnMut(&DerivationRecipe, &crate::ResolvedPair) -> Result<NonTextRecipeEvaluation, String>,
{
    let Some(policy) = recipe.substitution_policy() else {
        if recipe.substitutable_slot.is_some() {
            errors.push(DesignDiagnostic::error(
                "missing-compiled-substitution-policy",
                path,
                format!(
                    "recipe `{}` retained a substitutable slot without its compiled pair policy",
                    recipe.name
                ),
            ));
        }
        return;
    };
    let slot = policy.slot();
    let Some(RecipeBinding::Pair {
        name: authored_pair,
    }) = recipe.bindings.get(slot)
    else {
        errors.push(DesignDiagnostic::error(
            "invalid-derivation-signature",
            path,
            format!(
                "`{}` retained an invalid substitutable binding at authored argument {slot}",
                recipe.name
            ),
        ));
        return;
    };

    for pair_name in colours.pairs.keys() {
        let Some(decision) = policy.decision(pair_name) else {
            errors.push(DesignDiagnostic::error(
                "incomplete-compiled-substitution-policy",
                path,
                format!(
                    "recipe `{}` compiled pair policy does not classify `{pair_name}`",
                    recipe.name
                ),
            ));
            continue;
        };
        if matches!(decision, PairRefDecision::Excluded(_)) {
            continue;
        }
        // The eager authored execution already evaluated this exact input
        // identity. With the current registry, every Pair-bound cell recipe
        // has a compiled policy, so substituting its authored PairRef recreates
        // the eager painted pair. The only no-slot Pair recipes are the
        // primitive-only `control_pair` and `selection_pair`; their synthetic
        // cell identity is not a dictionary key, and `ring-surface-binding`
        // rejects a coupled derived ring before this walker. That exact
        // coupling is what makes this skip sound; a future no-slot Pair recipe
        // with a Pair binding must revisit it.
        if pair_name == authored_pair {
            continue;
        }
        let mut substituted = recipe.clone();
        let RecipeBinding::Pair { name } = &mut substituted.bindings[slot] else {
            unreachable!("the substitutable binding was checked above")
        };
        *name = pair_name.clone();
        let (fault, postcondition_name) = match substituted.output {
            RecipeOutput::Pair => (
                match evaluate_pair_recipe(&substituted, colours) {
                    Ok(evaluation) => {
                        verify_text_postcondition(&substituted, &evaluation.pair).err()
                    }
                    Err(message) => Some(message),
                },
                "text-contrast",
            ),
            RecipeOutput::NonText => (
                match painted_pair_for_override(pair_name, colours, cell_pair_derivation) {
                    None => None,
                    Some(painted_pair) => match evaluate_non_text(&substituted, &painted_pair) {
                        Ok(evaluation) => {
                            if let Some(warning) = evaluation.warning {
                                push_unique_warning(warnings, warning.code, path, warning.message);
                            }
                            verify_override_non_text_output(
                                &substituted,
                                evaluation.value,
                                &painted_pair,
                            )
                            .err()
                        }
                        Err(message) => Some(message),
                    },
                },
                "non-text-contrast",
            ),
        };
        if let Some(message) = fault {
            errors.push(DesignDiagnostic::error(
                "override-product",
                path,
                format!(
                    "recipe `{}` admitted pair `{pair_name}` at argument {slot}, but the permitted override failed its {postcondition_name} postcondition ({}): {message}",
                    recipe.name,
                    format_other_bindings(recipe, slot),
                ),
            ));
        }
    }
}

fn painted_pair_for_override(
    pair_name: &str,
    colours: &ResolvedColours,
    cell_pair_derivation: Option<&DerivationRecipe>,
) -> Option<crate::ResolvedPair> {
    let Some(recipe) = cell_pair_derivation else {
        return colours.pairs.get(pair_name).cloned();
    };
    let Some(policy) = recipe.substitution_policy() else {
        // SPEC 19 §9.3/§10.2 makes a direct override replace the fixed
        // derivation's eager output whole. The coupled ring therefore binds to
        // and is checked against the dictionary pair selected by that override.
        return colours.pairs.get(pair_name).cloned();
    };
    // `recipe` reached ring assembly only after `evaluate_recipe_for_cell`
    // finalised its total policy. Any missing classification emitted an error
    // there and the `?` in `assemble_cell` returned before the ring path, so a
    // missing key here is an internal invariant violation rather than authored
    // validation input.
    match policy
        .decision(pair_name)
        .expect("a compiled pair policy must classify every dictionary pair")
    {
        PairRefDecision::Excluded(_) => return None,
        PairRefDecision::Admitted => {}
    }

    let mut substituted = recipe.clone();
    let slot = policy.slot();
    let RecipeBinding::Pair { name } = &mut substituted.bindings[slot] else {
        unreachable!("the compiled pair policy slot must retain a pair binding")
    };
    *name = pair_name.to_owned();
    // The pair recipe's own product walk owns evaluator and text-postcondition
    // diagnostics. A non-text walk only consumes its successful painted value,
    // avoiding a second diagnostic for the same failed pair override.
    let evaluation = evaluate_pair_recipe(&substituted, colours).ok()?;
    verify_text_postcondition(&substituted, &evaluation.pair).ok()?;
    Some(evaluation.pair)
}

fn verify_override_non_text_output(
    recipe: &DerivationRecipe,
    output: crate::LinearRgba,
    surface: &crate::ResolvedPair,
) -> Result<(), String> {
    verify_non_text_postcondition(recipe, output, surface.rendered_surface)
}

pub(crate) fn push_unique_warning(
    warnings: &mut Vec<DesignDiagnostic>,
    code: &'static str,
    path: &str,
    message: String,
) {
    if warnings
        .iter()
        .any(|diagnostic| diagnostic.code == code && diagnostic.message == message)
    {
        return;
    }
    warnings.push(DesignDiagnostic::warning(code, path, message));
}

pub(crate) fn push_unique_error(
    errors: &mut Vec<DesignDiagnostic>,
    code: &'static str,
    path: &str,
    message: String,
) {
    if errors
        .iter()
        .any(|diagnostic| diagnostic.code == code && diagnostic.message == message)
    {
        return;
    }
    errors.push(DesignDiagnostic::error(code, path, message));
}

fn format_other_bindings(recipe: &DerivationRecipe, substituted_slot: usize) -> String {
    let mut bindings = recipe
        .bindings
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != substituted_slot)
        .map(|(index, binding)| match binding {
            RecipeBinding::Pair { name } => format!("arg[{index}]=pair `{name}`"),
            RecipeBinding::Colour { name, .. } => format!("arg[{index}]=colour `{name}`"),
            RecipeBinding::ColourList { names, .. } => {
                format!("arg[{index}]=colours [{}]", names.join(", "))
            }
            RecipeBinding::Ratio { name, value } => {
                format!("arg[{index}]=ratio `{name}` ({value})")
            }
        })
        .collect::<Vec<_>>();
    bindings.extend(
        recipe
            .implicit_bindings
            .iter()
            .map(|binding| format!("implicit={binding:?}")),
    );
    if bindings.is_empty() {
        "no other bindings".into()
    } else {
        bindings.join(", ")
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_pair_recipe_call<F>(
    name: &str,
    args: &[RecipeArgumentSource],
    context: DesignContext,
    colours: &ResolvedColours,
    registry: &[RecipeSignature],
    path: &str,
    resolve_ratio: F,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<DerivationRecipe>
where
    F: Fn(&str) -> Result<f64, String>,
{
    let signature = validate_pair_recipe_call(name, args, registry, path, errors)?;
    compile_recipe_bindings(
        signature,
        args,
        context,
        colours,
        path,
        resolve_ratio,
        errors,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compile_non_text_recipe_call<F>(
    name: &str,
    args: &[RecipeArgumentSource],
    context: DesignContext,
    colours: &ResolvedColours,
    registry: &[RecipeSignature],
    path: &str,
    resolve_ratio: F,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<DerivationRecipe>
where
    F: Fn(&str) -> Result<f64, String>,
{
    let signature = validate_non_text_recipe_call(name, args, registry, path, errors)?;
    compile_recipe_bindings(
        signature,
        args,
        context,
        colours,
        path,
        resolve_ratio,
        errors,
    )
}

fn compile_recipe_bindings<F>(
    signature: &RecipeSignature,
    args: &[RecipeArgumentSource],
    context: DesignContext,
    colours: &ResolvedColours,
    path: &str,
    resolve_ratio: F,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<DerivationRecipe>
where
    F: Fn(&str) -> Result<f64, String>,
{
    // Resolution deliberately follows the checked signature rather than
    // inferring intent from the source payload. This makes the tag comparison
    // in the role-specific call validator the one load-bearing kind guard: deleting
    // it makes a mis-tagged but otherwise valid argument compile, which its
    // mutation test detects.
    let mut bindings = Vec::with_capacity(args.len());
    for (argument, expected) in args.iter().zip(signature.params) {
        let binding = match expected {
            RecipeParam::Pair => {
                let value = argument_value(argument);
                if !colours.pairs.contains_key(value) {
                    errors.push(DesignDiagnostic::error(
                        "unknown-pair",
                        path,
                        format!("`{value}` is not a resolved semantic pair"),
                    ));
                    return None;
                }
                RecipeBinding::Pair { name: value.into() }
            }
            RecipeParam::Colour => {
                let name = argument_value(argument);
                let value = colours
                    .primitives
                    .get(name)
                    .copied()
                    .or_else(|| colours.non_text.get(name).map(|token| token.value));
                let Some(value) = value else {
                    errors.push(DesignDiagnostic::error(
                        "unknown-colour",
                        path,
                        format!("`{name}` is not a resolved colour token"),
                    ));
                    return None;
                };
                RecipeBinding::Colour {
                    name: name.into(),
                    value,
                }
            }
            RecipeParam::ColourList => {
                let names = argument_values(argument);
                let mut values = Vec::with_capacity(names.len());
                for name in names {
                    let Some(value) = colours.primitives.get(name).copied() else {
                        errors.push(DesignDiagnostic::error(
                            "unknown-colour",
                            path,
                            format!("`{name}` is not a resolved colour primitive"),
                        ));
                        return None;
                    };
                    values.push(value);
                }
                RecipeBinding::ColourList {
                    names: names.to_vec(),
                    values,
                }
            }
            RecipeParam::Ratio => {
                let name = argument_value(argument);
                let value = match resolve_ratio(name) {
                    Ok(value) => value,
                    Err(message) => {
                        errors.push(DesignDiagnostic::error(
                            "metric-kind-mismatch",
                            path,
                            message,
                        ));
                        return None;
                    }
                };
                RecipeBinding::Ratio {
                    name: name.into(),
                    value,
                }
            }
        };
        bindings.push(binding);
    }
    let implicit_bindings = signature
        .implicit_inputs
        .iter()
        .map(|input| match input {
            RecipeImplicitInput::ContextMode => RecipeImplicitBinding::ContextMode(context.mode),
        })
        .collect();
    Some(DerivationRecipe {
        name: signature.name,
        bindings,
        implicit_bindings,
        substitutable_slot: signature.substitutable_slot,
        movement: signature.movement,
        substitution_domain_constraints: signature.substitution_domain_constraints,
        output: signature.output,
        text_contrast_postcondition: signature.text_contrast_postcondition,
        non_text_contrast_postcondition: signature.non_text_contrast_postcondition,
        opaque_input_precondition: signature.opaque_input_precondition,
        opaque_output_invariant: signature.opaque_output_invariant,
        substitution_policy: None,
    })
}

pub(crate) fn validate_pair_recipe_call<'a>(
    name: &str,
    args: &[RecipeArgumentSource],
    registry: &'a [RecipeSignature],
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<&'a RecipeSignature> {
    let Some(signature) = registry.iter().find(|signature| signature.name == name) else {
        errors.push(DesignDiagnostic::error(
            "unregistered-derivation",
            path,
            format!("`{name}` is not a registered derivation"),
        ));
        return None;
    };
    if signature.output != RecipeOutput::Pair {
        errors.push(DesignDiagnostic::error(
            "derivation-output",
            path,
            format!("`{name}` does not produce a text pair"),
        ));
        return None;
    }
    if !signature.text_contrast_postcondition {
        errors.push(DesignDiagnostic::error(
            "derivation-without-text-contrast-postcondition",
            path,
            format!("`{name}` cannot be used in a text-bearing contrast role"),
        ));
        return None;
    }
    validate_recipe_call_structure(signature, args, path, errors)
}

pub(crate) fn validate_non_text_recipe_call<'a>(
    name: &str,
    args: &[RecipeArgumentSource],
    registry: &'a [RecipeSignature],
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<&'a RecipeSignature> {
    let Some(signature) = registry.iter().find(|signature| signature.name == name) else {
        errors.push(DesignDiagnostic::error(
            "unregistered-derivation",
            path,
            format!("`{name}` is not a registered derivation"),
        ));
        return None;
    };
    if signature.output != RecipeOutput::NonText {
        errors.push(DesignDiagnostic::error(
            "derivation-output",
            path,
            format!("`{name}` does not produce a non-text colour"),
        ));
        return None;
    }
    if !signature.non_text_contrast_postcondition {
        errors.push(DesignDiagnostic::error(
            "derivation-without-non-text-contrast-postcondition",
            path,
            format!("`{name}` cannot be used in a non-text contrast role"),
        ));
        return None;
    }
    validate_recipe_call_structure(signature, args, path, errors)
}

fn validate_recipe_call_structure<'a>(
    signature: &'a RecipeSignature,
    args: &[RecipeArgumentSource],
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<&'a RecipeSignature> {
    let name = signature.name;
    if !validate_signature(signature, path, errors) {
        return None;
    }
    if args.len() != signature.params.len() {
        errors.push(DesignDiagnostic::error(
            "derivation-arity",
            path,
            format!(
                "`{name}` expects {} authored arguments but received {}",
                signature.params.len(),
                args.len()
            ),
        ));
        return None;
    }
    for (index, (argument, expected)) in args.iter().zip(signature.params).enumerate() {
        if argument.param() != *expected {
            errors.push(DesignDiagnostic::error(
                "derivation-argument-kind",
                path,
                format!(
                    "`{name}` argument {index} is tagged `{:?}` but the signature requires `{:?}`",
                    argument.param(),
                    expected
                ),
            ));
            return None;
        }
    }
    for constraint in signature.cardinality_constraints {
        let RecipeArgumentSource::ColourList { values } = &args[constraint.param_index] else {
            unreachable!("validated cardinality constraint must target a colour list")
        };
        if values.len() < constraint.minimum {
            errors.push(DesignDiagnostic::error(
                "derivation-argument-cardinality",
                path,
                format!(
                    "`{name}` argument {} requires at least {} colour value(s) but received {}",
                    constraint.param_index,
                    constraint.minimum,
                    values.len()
                ),
            ));
            return None;
        }
    }

    Some(signature)
}

fn validate_signature(
    signature: &RecipeSignature,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> bool {
    let mut valid = true;
    let binds_base_pair = signature.params.contains(&RecipeParam::Pair);
    let movement_fault = match signature.output {
        RecipeOutput::Pair => match (
            binds_base_pair,
            signature.movement,
            signature.substitutable_slot,
        ) {
            (true, RecipeMovement::None, _) => Some("binds a base pair but declares no movement"),
            (false, RecipeMovement::Surface | RecipeMovement::Foreground, _) => {
                Some("is a constructor without a base pair but declares movement")
            }
            (true, RecipeMovement::Surface | RecipeMovement::Foreground, None) => {
                Some("declares movement without a substitutable base-pair slot")
            }
            _ => None,
        },
        RecipeOutput::NonText if signature.movement != RecipeMovement::None => {
            Some("produces non-text output but declares pair-member movement")
        }
        RecipeOutput::NonText => None,
    };
    if let Some(message) = movement_fault {
        errors.push(DesignDiagnostic::error(
            "invalid-derivation-signature",
            path,
            format!("`{}` {message}", signature.name),
        ));
        valid = false;
    }
    if let Some(slot) = signature.substitutable_slot {
        let total_inputs = signature.params.len() + signature.implicit_inputs.len();
        let fault = if slot >= total_inputs {
            Some("the substitutable slot is outside the signature's complete input list")
        } else if slot >= signature.params.len() {
            Some("the substitutable slot points at a compiler-supplied implicit input")
        } else if signature.params[slot] != RecipeParam::Pair {
            Some("the substitutable authored argument is not a pair")
        } else {
            None
        };
        if let Some(message) = fault {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                path,
                format!("`{}` {message}", signature.name),
            ));
            valid = false;
        }
    }

    let output_obligation_fault = match signature.output {
        RecipeOutput::Pair
            if signature.non_text_contrast_postcondition
                || signature.opaque_input_precondition.is_some()
                || signature.opaque_output_invariant =>
        {
            Some("declares non-text obligations on pair output")
        }
        RecipeOutput::NonText if signature.text_contrast_postcondition => {
            Some("declares a text postcondition on non-text output")
        }
        _ => None,
    };
    if let Some(message) = output_obligation_fault {
        errors.push(DesignDiagnostic::error(
            "invalid-derivation-signature",
            path,
            format!("`{}` {message}", signature.name),
        ));
        valid = false;
    }

    if let Some(index) = signature.opaque_input_precondition {
        let fault = if index >= signature.params.len() {
            Some("has an opaque-input precondition outside the authored argument list")
        } else if signature.params[index] != RecipeParam::Colour {
            Some("has an opaque-input precondition on a non-colour argument")
        } else {
            None
        };
        if let Some(message) = fault {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                path,
                format!("`{}` {message}", signature.name),
            ));
            valid = false;
        }
    }

    let mut domain_constrained_params = BTreeSet::new();
    for constraint in signature.substitution_domain_constraints {
        let fault = if !domain_constrained_params.insert(constraint.param_index) {
            Some("repeats a substitution domain constraint for one authored argument")
        } else if signature.substitutable_slot != Some(constraint.param_index) {
            Some("has a domain constraint that does not target its substitutable slot")
        } else if constraint.param_index >= signature.params.len() {
            Some("has a domain constraint outside the authored argument list")
        } else if signature.params[constraint.param_index] != RecipeParam::Pair {
            Some("has a domain constraint on a non-pair argument")
        } else {
            None
        };
        if let Some(message) = fault {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                path,
                format!("`{}` {message}", signature.name),
            ));
            valid = false;
        }
    }

    let mut constrained_params = BTreeSet::new();
    for constraint in signature.cardinality_constraints {
        let fault = if !constrained_params.insert(constraint.param_index) {
            Some("repeats a cardinality constraint for one authored argument")
        } else if constraint.minimum == 0 {
            Some("declares a zero minimum cardinality")
        } else if constraint.param_index >= signature.params.len() {
            Some("has a cardinality constraint outside the authored argument list")
        } else if signature.params[constraint.param_index] != RecipeParam::ColourList {
            Some("has a cardinality constraint on a non-list argument")
        } else {
            None
        };
        if let Some(message) = fault {
            errors.push(DesignDiagnostic::error(
                "invalid-derivation-signature",
                path,
                format!("`{}` {message}", signature.name),
            ));
            valid = false;
        }
    }
    valid
}

fn argument_value(argument: &RecipeArgumentSource) -> &str {
    match argument {
        RecipeArgumentSource::Pair { value }
        | RecipeArgumentSource::Colour { value }
        | RecipeArgumentSource::Ratio { value } => value,
        RecipeArgumentSource::ColourList { values } => {
            values.first().map(String::as_str).unwrap_or("")
        }
    }
}

fn argument_values(argument: &RecipeArgumentSource) -> &[String] {
    match argument {
        RecipeArgumentSource::ColourList { values } => values,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{LinearRgba, ResolvedPair};

    fn transparent_pair() -> ResolvedPair {
        ResolvedPair {
            surface_name: "transparent".into(),
            surface: LinearRgba {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 0.0,
            },
            foreground_name: "black".into(),
            foreground: LinearRgba::BLACK,
            backdrop_name: Some("white".into()),
            backdrop: Some(LinearRgba::WHITE),
            rendered_surface: LinearRgba::WHITE,
            rendered_foreground: LinearRgba::BLACK,
            contrast_ratio: 21.0,
            recipe: None,
        }
    }

    fn pending_toward(pair_name: &str, lift: f64) -> DerivationRecipe {
        let signature = crate::REGISTRY
            .iter()
            .find(|signature| signature.name == "contrast_safe_toward")
            .expect("toward signature");
        DerivationRecipe {
            name: signature.name,
            bindings: vec![
                RecipeBinding::Pair {
                    name: pair_name.into(),
                },
                RecipeBinding::Ratio {
                    name: "lift".into(),
                    value: lift,
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

    #[test]
    fn an_empty_compiled_admitted_set_is_rejected() {
        let colours = ResolvedColours {
            pairs: BTreeMap::from([("muted".into(), transparent_pair())]),
            ..Default::default()
        };
        let mut errors = Vec::new();
        let result = compile_pair_substitution_policy(
            &pending_toward("muted", 0.06),
            &colours,
            "test.empty-domain",
            &mut errors,
        );
        assert_eq!(result, None);
        assert!(errors.iter().any(|diagnostic| {
            diagnostic.code == "empty-override-product-domain"
                && diagnostic.message.contains("admits no pair")
        }));
    }

    #[test]
    fn product_walking_obeys_the_compiled_decision_after_dictionary_values_change() {
        let mut colours = ResolvedColours {
            pairs: BTreeMap::from([
                (
                    "base".into(),
                    ResolvedPair {
                        surface_name: "black".into(),
                        surface: LinearRgba::BLACK,
                        foreground_name: "white".into(),
                        foreground: LinearRgba::WHITE,
                        backdrop_name: None,
                        backdrop: None,
                        rendered_surface: LinearRgba::BLACK,
                        rendered_foreground: LinearRgba::WHITE,
                        contrast_ratio: 21.0,
                        recipe: None,
                    },
                ),
                ("muted".into(), transparent_pair()),
            ]),
            ..Default::default()
        };
        let mut errors = Vec::new();
        let finalized = compile_pair_substitution_policy(
            &pending_toward("base", 0.0),
            &colours,
            "test.policy-authority",
            &mut errors,
        )
        .expect("base keeps the policy non-empty");
        assert_eq!(
            finalized
                .substitution_policy()
                .expect("finalised policy")
                .decision("muted"),
            Some(&PairRefDecision::Excluded(
                PairRefExclusion::OutsideRecipeDomain {
                    required: crate::RecipePairDomain::NonTransparentSurface,
                }
            ))
        );

        let muted = colours.pairs.get_mut("muted").expect("muted pair");
        muted.surface = LinearRgba::BLACK;
        muted.rendered_surface = LinearRgba::BLACK;
        let mut warnings = Vec::new();
        validate_override_product(
            &finalized,
            &colours,
            None,
            "test.policy-authority",
            &mut warnings,
            &mut errors,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    fn focus_recipe(seed: LinearRgba, pair_name: &str) -> DerivationRecipe {
        let signature = crate::REGISTRY
            .iter()
            .find(|signature| signature.name == "focus_ring")
            .unwrap();
        DerivationRecipe {
            name: signature.name,
            bindings: vec![
                RecipeBinding::Colour {
                    name: "ring".into(),
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

    fn opaque_surface(name: &str, value: LinearRgba) -> ResolvedPair {
        ResolvedPair {
            surface_name: name.into(),
            surface: value,
            foreground_name: "white".into(),
            foreground: LinearRgba::WHITE,
            backdrop_name: None,
            backdrop: None,
            rendered_surface: value,
            rendered_foreground: LinearRgba::WHITE,
            contrast_ratio: crate::contrast_ratio(LinearRgba::WHITE, value),
            recipe: None,
        }
    }

    #[test]
    fn non_text_override_product_reexecutes_against_the_painted_surface() {
        let seed =
            crate::colour_model::derivation::oklch_to_linear_srgb(0.8, 0.0, 0.0, 1.0).unwrap();
        let primary_surface =
            crate::colour_model::derivation::oklch_to_linear_srgb(0.79, 0.0, 0.0, 1.0).unwrap();
        let colours = ResolvedColours {
            pairs: BTreeMap::from([
                (
                    "secondary".into(),
                    opaque_surface("secondary", LinearRgba::BLACK),
                ),
                ("primary".into(), opaque_surface("primary", primary_surface)),
            ]),
            ..Default::default()
        };
        let pending = focus_recipe(seed, "secondary");
        let mut errors = Vec::new();
        let finalized =
            compile_pair_substitution_policy(&pending, &colours, "test.override-ring", &mut errors)
                .unwrap();
        let mut warnings = Vec::new();
        let mut evaluated_surfaces = Vec::new();
        validate_override_product_with_non_text_evaluator(
            &finalized,
            &colours,
            None,
            "test.override-ring",
            &mut warnings,
            &mut errors,
            |recipe, painted_pair| {
                evaluated_surfaces.push(painted_pair.surface_name.clone());
                evaluate_non_text_recipe_against(recipe, painted_pair)
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            evaluated_surfaces,
            ["primary"],
            "the override walker must invoke the NonText evaluator with the replacement's painted surface"
        );
    }

    #[test]
    fn override_non_text_output_opacity_checker_is_not_eager_only() {
        let colours = ResolvedColours {
            pairs: BTreeMap::from([
                (
                    "primary".into(),
                    opaque_surface("primary", LinearRgba::BLACK),
                ),
                (
                    "secondary".into(),
                    opaque_surface("secondary", LinearRgba::BLACK),
                ),
            ]),
            ..Default::default()
        };
        let pending = focus_recipe(LinearRgba::WHITE, "primary");
        let mut errors = Vec::new();
        let finalized = compile_pair_substitution_policy(
            &pending,
            &colours,
            "test.override-opacity",
            &mut errors,
        )
        .unwrap();
        let translucent = LinearRgba {
            alpha: 0.5,
            ..LinearRgba::WHITE
        };
        let mut warnings = Vec::new();
        validate_override_product_with_non_text_evaluator(
            &finalized,
            &colours,
            None,
            "test.override-opacity",
            &mut warnings,
            &mut errors,
            |_, _| {
                Ok(NonTextRecipeEvaluation {
                    value: translucent,
                    provenance: crate::FocusRingProvenance {
                        seed_name: "ring".into(),
                        step_index: 0,
                        delta_l: 0.0,
                    },
                    warning: None,
                })
            },
        );
        assert_eq!(errors.len(), 1, "override checker was bypassed: {errors:?}");
        assert_eq!(errors[0].code, "override-product");
        assert!(errors[0].message.contains("opaque output is invariant"));
    }
}
