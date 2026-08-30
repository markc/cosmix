use std::collections::{BTreeMap, BTreeSet};

use crate::DesignContext;
use crate::colour_model::derivation::{
    composite, evaluate_pair_recipe, oklch_to_linear_srgb, verify_text_postcondition,
};
use crate::colour_model::{
    LinearRgba, NON_TEXT_NAMES, ResolvedColours, ResolvedNonTextColour, ResolvedPair,
    TEXT_PAIR_NAMES, contrast_ratio,
};
use crate::diagnostic::{CompileSuccess, DesignDiagnostic};
use crate::recipe::{REGISTRY, RecipeSignature};
use crate::recipe_compiler::{
    compile_pair_recipe_call, compile_pair_substitution_policy, validate_override_product,
    validate_pair_recipe_call, validate_recipe_registry,
};
use crate::source::{DesignV1Source, MetricSource, PairSource, RecipeArgumentSource};

/// The guaranteed-AA black-or-white foreground §3.4 offers as a *suggestion*
/// when a pair fails the gate. This lives on the compiler side, not in
/// `derivation`: it is not a §10.2 recipe and no resolver re-executes it — it
/// only ever decorates a compile diagnostic.
fn guaranteed_knockout(surface: LinearRgba) -> LinearRgba {
    if contrast_ratio(LinearRgba::BLACK, surface) >= contrast_ratio(LinearRgba::WHITE, surface) {
        LinearRgba::BLACK
    } else {
        LinearRgba::WHITE
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColourCompileFailure {
    pub diagnostics: Vec<DesignDiagnostic>,
}

pub fn compile_colour_tokens(
    source: &DesignV1Source,
    context: DesignContext,
) -> Result<CompileSuccess<ResolvedColours>, ColourCompileFailure> {
    compile_colour_tokens_with_registry(source, context, REGISTRY)
}

pub(crate) fn compile_colour_tokens_with_registry(
    source: &DesignV1Source,
    context: DesignContext,
    registry: &[RecipeSignature],
) -> Result<CompileSuccess<ResolvedColours>, ColourCompileFailure> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    validate_recipe_registry(registry, "compiler.derivations", &mut errors);
    let primitives = compile_primitives(source, &mut errors);

    validate_closed_vocabulary(
        source.semantics.pairs.keys().map(String::as_str),
        &TEXT_PAIR_NAMES,
        "design.v1.semantics.pairs",
        &mut errors,
    );
    validate_closed_vocabulary(
        source.semantics.non_text.keys().map(String::as_str),
        &NON_TEXT_NAMES,
        "design.v1.semantics.non_text",
        &mut errors,
    );

    let mut pairs = BTreeMap::new();
    for (name, pair) in &source.semantics.pairs {
        let PairSource::Authored(pair) = pair else {
            continue;
        };
        let path = format!("design.v1.semantics.pairs.{name}");
        let Some(surface) = primitive_ref(&primitives, &pair.surface, &path, &mut errors) else {
            continue;
        };
        let Some(foreground) = primitive_ref(&primitives, &pair.foreground, &path, &mut errors)
        else {
            continue;
        };
        let backdrop = pair
            .backdrop
            .as_deref()
            .and_then(|value| primitive_ref(&primitives, value, &path, &mut errors));

        if (!surface.opaque() || !foreground.opaque()) && pair.backdrop.is_none() {
            errors.push(DesignDiagnostic::error(
                "translucent-pair-without-backdrop",
                path,
                "a translucent text-bearing pair must declare a primitive backdrop",
            ));
            continue;
        }
        if pair.backdrop.is_some() && backdrop.is_none() {
            continue;
        }

        let rendered_surface = backdrop.map_or(surface, |under| composite(surface, under));
        let rendered_foreground = if foreground.opaque() {
            foreground
        } else {
            composite(foreground, rendered_surface)
        };
        if !rendered_surface.opaque() || !rendered_foreground.opaque() {
            errors.push(DesignDiagnostic::error(
                "translucent-pair-without-opaque-composite",
                format!("design.v1.semantics.pairs.{name}"),
                "the declared backdrop must resolve the text pair to an opaque composite",
            ));
            continue;
        }
        let ratio = contrast_ratio(rendered_foreground, rendered_surface);
        if ratio < 4.5 {
            let knockout = guaranteed_knockout(rendered_surface);
            errors.push(
                DesignDiagnostic::error(
                    "text-contrast",
                    format!("design.v1.semantics.pairs.{name}"),
                    format!("pair contrast {ratio:.3}:1 is below WCAG AA 4.5:1"),
                )
                .with_suggestion(format!(
                    "use opaque {} as the foreground (guaranteed {:.3}:1)",
                    if knockout == LinearRgba::BLACK {
                        "black"
                    } else {
                        "white"
                    },
                    contrast_ratio(knockout, rendered_surface)
                )),
            );
            continue;
        }
        pairs.insert(
            name.clone(),
            ResolvedPair {
                surface_name: pair.surface.clone(),
                surface,
                foreground_name: pair.foreground.clone(),
                foreground,
                backdrop_name: pair.backdrop.clone(),
                backdrop,
                rendered_surface,
                rendered_foreground,
                contrast_ratio: ratio,
                recipe: None,
            },
        );
    }

    compile_derived_pairs(
        source,
        context,
        &primitives,
        &mut pairs,
        &mut warnings,
        &mut errors,
        registry,
    );
    if errors.is_empty() {
        finalize_semantic_override_products(&primitives, &mut pairs, &mut warnings, &mut errors);
    }

    let mut non_text = BTreeMap::new();
    for (name, token) in &source.semantics.non_text {
        let path = format!("design.v1.semantics.non_text.{name}");
        let Some(value) = primitive_ref(&primitives, &token.value, &path, &mut errors) else {
            continue;
        };
        if token.adjacent.is_empty() {
            errors.push(DesignDiagnostic::error(
                "missing-adjacency",
                &path,
                "non-text colours must declare at least one adjacent semantic pair",
            ));
            continue;
        }
        let mut adjacent = BTreeSet::new();
        for pair_name in &token.adjacent {
            let Some(pair) = pairs.get(pair_name) else {
                errors.push(DesignDiagnostic::error(
                    "unknown-adjacent-pair",
                    &path,
                    format!("adjacent pair `{pair_name}` is not a resolved semantic pair"),
                ));
                continue;
            };
            adjacent.insert(pair_name.clone());
            let rendered = if value.opaque() {
                value
            } else {
                composite(value, pair.rendered_surface)
            };
            let ratio = contrast_ratio(rendered, pair.rendered_surface);
            if ratio < 3.0 {
                let message = format!(
                    "`{name}` contrast against `{pair_name}` is {ratio:.3}:1; expected 3:1"
                );
                let diagnostic = if name == "ring" {
                    DesignDiagnostic::error("non-text-contrast", &path, message)
                } else {
                    DesignDiagnostic::warning("non-text-contrast", &path, message)
                };
                if name == "ring" {
                    errors.push(diagnostic);
                } else {
                    warnings.push(diagnostic);
                }
            }
        }
        non_text.insert(
            name.clone(),
            ResolvedNonTextColour {
                value_name: token.value.clone(),
                value,
                adjacent,
            },
        );
    }

    if errors.is_empty() {
        Ok(CompileSuccess {
            value: ResolvedColours {
                primitives,
                pairs,
                non_text,
            },
            diagnostics: warnings,
        })
    } else {
        warnings.extend(errors);
        Err(ColourCompileFailure {
            diagnostics: warnings,
        })
    }
}

fn finalize_semantic_override_products(
    primitives: &BTreeMap<String, LinearRgba>,
    pairs: &mut BTreeMap<String, ResolvedPair>,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
) {
    let colours = ResolvedColours {
        primitives: primitives.clone(),
        pairs: pairs.clone(),
        non_text: BTreeMap::new(),
    };
    let mut finalized = Vec::<(crate::DerivationRecipe, Option<crate::DerivationRecipe>)>::new();
    for name in pairs.keys().cloned().collect::<Vec<_>>() {
        let Some(recipe) = pairs[&name].recipe.clone() else {
            continue;
        };
        let finalized_recipe = if let Some((_, finalized_recipe)) = finalized
            .iter()
            .find(|(pending_recipe, _)| pending_recipe == &recipe)
        {
            finalized_recipe.clone()
        } else {
            let path = format!("design.v1.semantics.pairs.{name}");
            let finalized_recipe =
                compile_pair_substitution_policy(&recipe, &colours, &path, errors);
            if let Some(recipe) = finalized_recipe.as_ref() {
                validate_override_product(recipe, &colours, None, &path, warnings, errors);
            }
            finalized.push((recipe.clone(), finalized_recipe.clone()));
            finalized_recipe
        };
        if let Some(finalized_recipe) = finalized_recipe {
            pairs
                .get_mut(&name)
                .expect("derived semantic pair remains present during finalisation")
                .recipe = Some(finalized_recipe);
        }
    }
}

fn compile_derived_pairs(
    source: &DesignV1Source,
    context: DesignContext,
    primitives: &BTreeMap<String, LinearRgba>,
    pairs: &mut BTreeMap<String, ResolvedPair>,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    registry: &[RecipeSignature],
) {
    let mut pending = BTreeMap::new();
    for (name, pair) in &source.semantics.pairs {
        let PairSource::Derived { derive } = pair else {
            continue;
        };
        let path = format!("design.v1.semantics.pairs.{name}");
        if validate_pair_recipe_call(&derive.name, &derive.args, registry, &path, errors).is_some()
        {
            pending.insert(name.clone(), derive);
        }
    }
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .filter(|(_, call)| pair_dependencies_ready(&call.args, &source.semantics.pairs, pairs))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if ready.is_empty() {
            for (name, call) in pending {
                let dependencies = call
                    .args
                    .iter()
                    .filter_map(|argument| match argument {
                        RecipeArgumentSource::Pair { value } if !pairs.contains_key(value) => {
                            Some(value.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                errors.push(DesignDiagnostic::error(
                    "unresolved-derived-pair",
                    format!("design.v1.semantics.pairs.{name}"),
                    format!(
                        "derived semantic pair `{name}` has unresolved or cyclic pair dependencies: {dependencies}"
                    ),
                ));
            }
            break;
        }
        for name in ready {
            let call = pending
                .remove(&name)
                .expect("ready derived pair remains pending");
            let path = format!("design.v1.semantics.pairs.{name}");
            let colours = ResolvedColours {
                primitives: primitives.clone(),
                pairs: pairs.clone(),
                non_text: BTreeMap::new(),
            };
            let Some(recipe) = compile_pair_recipe_call(
                &call.name,
                &call.args,
                context.clone(),
                &colours,
                registry,
                &path,
                |ratio_name| resolve_source_ratio(source, ratio_name),
                errors,
            ) else {
                continue;
            };
            let evaluation = match evaluate_pair_recipe(&recipe, &colours) {
                Ok(evaluation) => evaluation,
                Err(message) => {
                    errors.push(DesignDiagnostic::error(
                        "derivation-evaluation",
                        &path,
                        message,
                    ));
                    continue;
                }
            };
            if let Some(warning) = evaluation.warning {
                warnings.push(DesignDiagnostic::warning(
                    warning.code,
                    &path,
                    warning.message,
                ));
            }
            if let Err(message) = verify_text_postcondition(&recipe, &evaluation.pair) {
                let knockout = guaranteed_knockout(evaluation.pair.rendered_surface);
                errors.push(
                    DesignDiagnostic::error("derivation-text-postcondition", &path, message)
                        .with_suggestion(format!(
                            "use opaque {} as the foreground (guaranteed {:.3}:1)",
                            if knockout == LinearRgba::BLACK {
                                "black"
                            } else {
                                "white"
                            },
                            contrast_ratio(knockout, evaluation.pair.rendered_surface)
                        )),
                );
                continue;
            }
            pairs.insert(name, evaluation.pair);
        }
    }
}

fn pair_dependencies_ready(
    args: &[RecipeArgumentSource],
    declared: &BTreeMap<String, PairSource>,
    resolved: &BTreeMap<String, ResolvedPair>,
) -> bool {
    args.iter().all(|argument| match argument {
        RecipeArgumentSource::Pair { value } => {
            !declared.contains_key(value) || resolved.contains_key(value)
        }
        _ => true,
    })
}

fn resolve_source_ratio(source: &DesignV1Source, name: &str) -> Result<f64, String> {
    let Some(metric) = source.primitives.metrics.get(name) else {
        return Err(format!("`{name}` is not a metric primitive"));
    };
    let MetricSource::Tagged(metric) = metric else {
        return Err(format!("`{name}` is an untagged metric, not a ratio"));
    };
    if metric.kind != "ratio" || metric.scale.is_some() {
        return Err(format!("`{name}` is not a ratio metric"));
    }
    if !metric.value.is_finite() {
        return Err(format!("ratio metric `{name}` must be finite"));
    }
    Ok(metric.value)
}

fn compile_primitives(
    source: &DesignV1Source,
    errors: &mut Vec<DesignDiagnostic>,
) -> BTreeMap<String, LinearRgba> {
    let mut primitives = BTreeMap::new();
    for (name, value) in &source.primitives.colors {
        match oklch_to_linear_srgb(value.l, value.c, value.h, value.alpha) {
            Ok(value) => {
                primitives.insert(name.clone(), value);
            }
            Err(message) => errors.push(DesignDiagnostic::error(
                "invalid-oklch",
                format!("design.v1.primitives.colors.{name}"),
                message,
            )),
        }
    }
    primitives
}

fn primitive_ref(
    primitives: &BTreeMap<String, LinearRgba>,
    name: &str,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<LinearRgba> {
    let value = primitives.get(name).copied();
    if value.is_none() {
        errors.push(DesignDiagnostic::error(
            "unknown-primitive",
            path,
            format!("`{name}` is not a colour primitive"),
        ));
    }
    value
}

fn validate_closed_vocabulary<'a>(
    actual: impl Iterator<Item = &'a str>,
    required: &[&str],
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) {
    let actual = actual.collect::<BTreeSet<_>>();
    let required = required.iter().copied().collect::<BTreeSet<_>>();
    for missing in required.difference(&actual) {
        errors.push(DesignDiagnostic::error(
            "missing-semantic-token",
            path,
            format!("required semantic token `{missing}` is missing"),
        ));
    }
    for extra in actual.difference(&required) {
        errors.push(DesignDiagnostic::error(
            "unknown-semantic-token",
            path,
            format!("semantic token `{extra}` is outside the closed vocabulary"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticSeverity;
    use crate::source::{
        ColourSpace, DerivationCallSource, NonTextColourSource, OklchSource, PairSource,
        PrimitiveSource, RecipeArgumentSource, SemanticSource, SourceKind,
    };
    use crate::{Mode, RecipeImplicitBinding};

    fn fixture_source() -> DesignV1Source {
        let mut colors = BTreeMap::new();
        colors.insert("dark".into(), colour(0.2, 0.02, 250.0));
        colors.insert("light".into(), colour(0.95, 0.01, 250.0));
        colors.insert("ring".into(), colour(0.72, 0.18, 240.0));
        let pairs = TEXT_PAIR_NAMES
            .into_iter()
            .map(|name| (name.to_owned(), PairSource::authored("dark", "light", None)))
            .collect();
        let non_text = NON_TEXT_NAMES
            .into_iter()
            .map(|name| {
                (
                    name.to_owned(),
                    NonTextColourSource {
                        value: "ring".into(),
                        adjacent: vec!["base".into()],
                    },
                )
            })
            .collect();
        DesignV1Source {
            kind: SourceKind::Base,
            resolution_order: Vec::new(),
            modifiers: Vec::new(),
            primitives: PrimitiveSource {
                colors,
                metrics: BTreeMap::new(),
                scales: BTreeMap::new(),
            },
            semantics: SemanticSource { pairs, non_text },
            typography: Default::default(),
            families: Default::default(),
            v0_crosswalk: Default::default(),
        }
    }

    fn colour(l: f64, c: f64, h: f64) -> OklchSource {
        OklchSource {
            color_space: ColourSpace::Oklch,
            l,
            c,
            h,
            alpha: 1.0,
        }
    }

    fn compile(
        source: &DesignV1Source,
    ) -> Result<CompileSuccess<ResolvedColours>, ColourCompileFailure> {
        compile_colour_tokens(source, DesignContext::default())
    }

    fn colour_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Colour {
            value: value.into(),
        }
    }

    fn pair_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Pair {
            value: value.into(),
        }
    }

    fn colour_list_arg(values: &[&str]) -> RecipeArgumentSource {
        RecipeArgumentSource::ColourList {
            values: values.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn ratio_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Ratio {
            value: value.into(),
        }
    }

    #[test]
    fn wcag_reference_points_use_linear_delivery_values() {
        assert!((contrast_ratio(LinearRgba::BLACK, LinearRgba::WHITE) - 21.0).abs() < 1e-12);
        let grey = LinearRgba {
            red: 0.5,
            green: 0.5,
            blue: 0.5,
            alpha: 1.0,
        };
        assert!((contrast_ratio(grey, LinearRgba::WHITE) - 1.909).abs() < 0.001);
    }

    #[test]
    fn compiler_entry_rejects_an_invalid_recipe_registry() {
        let source = fixture_source();
        let invalid_registry = [REGISTRY[0], REGISTRY[0]];
        let failure = compile_colour_tokens_with_registry(
            &source,
            DesignContext::default(),
            &invalid_registry,
        )
        .expect_err("an invalid registry must fail colour compilation");
        assert!(failure.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid-derivation-signature"
                && diagnostic.path.starts_with("compiler.derivations.")
        }));
    }

    #[test]
    fn out_of_gamut_oklch_is_chroma_mapped_before_quantising() {
        let source = colour(0.7, 0.5, 30.0);
        let resolved = oklch_to_linear_srgb(source.l, source.c, source.h, source.alpha).unwrap();
        assert!(
            [resolved.red, resolved.green, resolved.blue]
                .into_iter()
                .all(|channel| (0.0..=1.0).contains(&channel))
        );
    }

    #[test]
    fn closed_semantic_vocabulary_is_enforced() {
        let mut source = fixture_source();
        source.semantics.pairs.remove("popover");
        source.semantics.pairs.insert(
            "background".into(),
            PairSource::authored("dark", "light", None),
        );
        let failure = compile(&source).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "missing-semantic-token")
        );
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "unknown-semantic-token")
        );
    }

    #[test]
    fn translucent_pairs_require_and_measure_their_backdrop() {
        let mut source = fixture_source();
        source.primitives.colors.get_mut("dark").unwrap().alpha = 0.5;
        let failure = compile(&source).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "translucent-pair-without-backdrop")
        );
    }

    #[test]
    fn translucent_backdrops_cannot_leave_the_measured_composite_translucent() {
        let mut source = fixture_source();
        source.primitives.colors.get_mut("dark").unwrap().alpha = 0.5;
        let mut under = colour(0.1, 0.01, 250.0);
        under.alpha = 0.5;
        source.primitives.colors.insert("under".into(), under);
        for pair in source.semantics.pairs.values_mut() {
            let PairSource::Authored(pair) = pair else {
                panic!("fixture pairs are authored")
            };
            pair.backdrop = Some("under".into());
        }
        let failure = compile(&source).unwrap_err();
        assert!(
            failure.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "translucent-pair-without-opaque-composite"
            })
        );
    }

    #[test]
    fn failing_text_pair_is_fatal_and_suggests_a_knockout() {
        let mut source = fixture_source();
        source.primitives.colors.get_mut("light").unwrap().l = 0.3;
        let failure = compile(&source).unwrap_err();
        let diagnostic = failure
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "text-contrast")
            .unwrap();
        assert!(diagnostic.suggestion.is_some());
    }

    #[test]
    fn invisible_ring_is_fatal_but_decorative_border_is_a_warning() {
        let mut source = fixture_source();
        source.semantics.non_text.get_mut("ring").unwrap().value = "dark".into();
        let failure = compile(&source).unwrap_err();
        assert!(failure.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "non-text-contrast"
                && diagnostic.path.ends_with("ring")
                && diagnostic.severity == DiagnosticSeverity::Error
        }));

        let mut source = fixture_source();
        source.semantics.non_text.get_mut("border").unwrap().value = "dark".into();
        let resolved = compile(&source).unwrap();
        assert!(resolved.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "non-text-contrast"
                && diagnostic.path.ends_with("border")
                && diagnostic.severity == DiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn fatal_compile_retains_a_warning_raised_before_the_error() {
        let mut source = fixture_source();
        source.semantics.non_text.get_mut("border").unwrap().value = "dark".into();
        source.semantics.non_text.get_mut("ring").unwrap().value = "dark".into();
        let failure = compile(&source).unwrap_err();
        assert!(failure.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == DiagnosticSeverity::Warning
                && diagnostic.path.ends_with("border")
        }));
        assert!(failure.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == DiagnosticSeverity::Error && diagnostic.path.ends_with("ring")
        }));
    }

    #[test]
    fn control_pair_is_a_derived_semantic_pair_and_a_legal_dictionary_target() {
        let mut source = fixture_source();
        source.semantics.pairs.insert(
            "secondary".into(),
            PairSource::Derived {
                derive: DerivationCallSource {
                    name: "control_pair".into(),
                    args: vec![
                        colour_arg("dark"),
                        colour_arg("ring"),
                        colour_arg("dark"),
                        colour_arg("light"),
                    ],
                },
            },
        );
        let compiled = compile(&source).expect("derived control pair compiles");
        let pair = &compiled.value.pairs["secondary"];
        let recipe = pair.recipe.as_ref().expect("semantic recipe retained");
        assert_eq!(recipe.name, "control_pair");
        assert_eq!(recipe.substitutable_slot, None);
        let (l, _, h) = crate::colour_model::derivation::linear_srgb_to_oklch(
            compiled.value.primitives["dark"],
        );
        let (_, c, _) = crate::colour_model::derivation::linear_srgb_to_oklch(
            compiled.value.primitives["ring"],
        );
        let expected = oklch_to_linear_srgb(l + 0.03, (c * 0.35).min(0.08), h, 1.0).unwrap();
        assert_eq!(pair.surface, expected);
        assert!(pair.contrast_ratio >= 4.5);
    }

    #[test]
    fn selection_pair_retains_the_compiled_mode_and_checks_its_actual_output() {
        let mut source = fixture_source();
        source.semantics.pairs.insert(
            "primary".into(),
            PairSource::Derived {
                derive: DerivationCallSource {
                    name: "selection_pair".into(),
                    args: vec![colour_arg("ring"), colour_arg("light")],
                },
            },
        );
        let dark = compile_colour_tokens(
            &source,
            DesignContext {
                mode: Mode::Dark,
                ..Default::default()
            },
        )
        .expect("dark selection compiles");
        let light = compile_colour_tokens(
            &source,
            DesignContext {
                mode: Mode::Light,
                ..Default::default()
            },
        )
        .expect("light selection compiles");
        let dark_pair = &dark.value.pairs["primary"];
        assert_eq!(
            dark_pair.recipe.as_ref().unwrap().implicit_bindings,
            [RecipeImplicitBinding::ContextMode(Mode::Dark)]
        );
        assert_ne!(dark_pair.surface, light.value.pairs["primary"].surface);
        assert!(dark_pair.contrast_ratio >= 4.5);
        assert!(light.value.pairs["primary"].contrast_ratio >= 4.5);
    }

    #[test]
    fn semantic_recipe_override_product_domain_excludes_only_the_transparent_substitution() {
        let mut source = fixture_source();
        let mut transparent = colour(0.0, 0.0, 0.0);
        transparent.alpha = 0.0;
        source
            .primitives
            .colors
            .insert("transparent".into(), transparent);
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.03));
        source
            .semantics
            .pairs
            .insert("base".into(), PairSource::authored("dark", "light", None));
        source.semantics.pairs.insert(
            "card".into(),
            PairSource::authored("transparent", "dark", Some("light".into())),
        );
        source.semantics.pairs.insert(
            "secondary".into(),
            PairSource::Derived {
                derive: DerivationCallSource {
                    name: "contrast_safe_state_pair".into(),
                    args: vec![
                        pair_arg("base"),
                        colour_list_arg(&["light"]),
                        ratio_arg("lift"),
                    ],
                },
            },
        );

        let success = compile(&source).expect(
            "visible authored lift passes; only the transparent card substitution is outside the domain",
        );
        let recipe = success.value.pairs["secondary"]
            .recipe
            .as_ref()
            .expect("derived semantic pair retains its finalised recipe");
        assert_eq!(
            recipe
                .substitution_policy()
                .expect("substitutable recipe retains its policy")
                .decision("card"),
            Some(&crate::PairRefDecision::Excluded(
                crate::PairRefExclusion::OutsideRecipeDomain {
                    required: crate::RecipePairDomain::NonTransparentSurface,
                }
            ))
        );
        assert!(
            !success
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "override-product-domain-exclusion")
        );
    }
}
