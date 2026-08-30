use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::{
    ButtonCellKey, ButtonPart, ButtonProperty, ButtonSize, ButtonTypographyKey, ButtonVariant,
    DesignApplyDecision, DesignCompileOutcome, DesignCompileResult, DesignContext,
    EMBEDDED_DEFAULT_SOURCE, InteractionState, MappingValueSource, Mode, ModifierAxis,
    PairOverrideDisposition, PairOverrideRoute, PairRefDecision, PairSource, RecipeArgumentSource,
    ResolvedDesign, Scheme, SourceIdentity, apply_compiled_design, compile_design, contrast_ratio,
    parse_design_source,
};

fn parsed_trial(identity: &str) -> crate::DesignSourceDocument {
    parse_design_source(SourceIdentity::new(identity), EMBEDDED_DEFAULT_SOURCE)
        .unwrap_or_else(|error| panic!("embedded default did not parse: {error}"))
}

fn compile_trial() -> ResolvedDesign {
    let transition = apply_compiled_design(
        None,
        compile_design(
            &parsed_trial("embedded:revision-1"),
            DesignContext::default(),
        ),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(
        transition.status.outcome,
        DesignCompileOutcome::SucceededWithWarnings,
        "{:#?}",
        transition.status.diagnostics
    );
    assert_eq!(transition.decision, DesignApplyDecision::Replaced);
    transition
        .design
        .expect("embedded default unexpectedly failed")
}

fn rgba(value: crate::LinearRgba) -> String {
    let [red, green, blue, alpha] = value.to_srgba8();
    format!("#{red:02X}{green:02X}{blue:02X}{alpha:02X}")
}

fn authored_colour(
    document: &crate::DesignSourceDocument,
    context: &DesignContext,
    name: &str,
) -> crate::OklchSource {
    let mut value = document.v1.primitives.colors[name];
    let mut selected = document
        .v1
        .modifiers
        .iter()
        .enumerate()
        .filter(|(_, block)| {
            block.when.iter().all(|(axis, expected)| {
                let actual = match axis {
                    ModifierAxis::Scheme => context.scheme.name(),
                    ModifierAxis::Mode => context.mode.name(),
                    ModifierAxis::Contrast => context.contrast.name(),
                    ModifierAxis::App => context.app.as_deref().unwrap_or(""),
                };
                actual == expected
            })
        })
        .collect::<Vec<_>>();
    selected.sort_by_key(|(index, block)| {
        let last_axis = block
            .when
            .keys()
            .filter_map(|axis| {
                document
                    .v1
                    .resolution_order
                    .iter()
                    .position(|candidate| candidate == axis)
            })
            .max()
            .expect("fixture modifier has a selected axis");
        (block.when.len(), last_axis, *index)
    });
    for (_, block) in selected {
        if let Some(replacement) = block.primitives.colors.get(name) {
            value = *replacement;
        }
    }
    value
}

fn assert_oklch(actual: crate::OklchSource, expected: [f64; 3]) {
    assert_eq!(
        [actual.l, actual.c, actual.h, actual.alpha],
        [expected[0], expected[1], expected[2], 1.0]
    );
}

#[test]
fn embedded_default_source_compiles_to_revision_one() {
    let artifact = compile_trial();
    assert_eq!(artifact.revision(), crate::EMBEDDED_DEFAULT_REVISION);
    assert_eq!(artifact.dictionary().colours.pairs.len(), 8);
    assert_eq!(artifact.dictionary().colours.non_text.len(), 3);
    assert_eq!(artifact.tables().button.len(), crate::BUTTON_CELL_COUNT);
    assert_eq!(
        artifact.typography().button_assignments().len(),
        crate::BUTTON_TYPOGRAPHY_COUNT
    );
}

#[test]
fn embedded_authored_role_anchors_match_revision_one_in_all_twelve_contexts() {
    const ROLE_NAMES: [&str; 7] = [
        "palette.background.1",
        "palette.background.2",
        "palette.background.3",
        "palette.foreground.default",
        "palette.foreground.muted",
        "palette.accent.default",
        "palette.accent.hover",
    ];
    let expected = [
        (
            Scheme::Ocean,
            Mode::Light,
            [
                [0.98, 0.008, 220.0],
                [0.96, 0.012, 220.0],
                [0.92, 0.018, 220.0],
                [0.25, 0.06, 220.0],
                [0.50, 0.06, 220.0],
                [0.50, 0.12, 220.0],
                [0.45, 0.14, 220.0],
            ],
        ),
        (
            Scheme::Ocean,
            Mode::Dark,
            [
                [0.12, 0.015, 220.0],
                [0.16, 0.02, 220.0],
                [0.22, 0.025, 220.0],
                [0.95, 0.02, 220.0],
                [0.61, 0.04, 220.0],
                [0.75, 0.12, 220.0],
                [0.85, 0.10, 220.0],
            ],
        ),
        (
            Scheme::Crimson,
            Mode::Light,
            [
                [0.98, 0.008, 25.0],
                [0.96, 0.012, 25.0],
                [0.92, 0.018, 25.0],
                [0.25, 0.04, 25.0],
                [0.50, 0.04, 25.0],
                [0.47, 0.20, 25.0],
                [0.42, 0.22, 25.0],
            ],
        ),
        (
            Scheme::Crimson,
            Mode::Dark,
            [
                [0.10, 0.015, 25.0],
                [0.14, 0.02, 25.0],
                [0.20, 0.025, 25.0],
                [0.95, 0.02, 25.0],
                [0.61, 0.03, 25.0],
                [0.63, 0.23, 25.0],
                [0.70, 0.25, 25.0],
            ],
        ),
        (
            Scheme::Stone,
            Mode::Light,
            [
                [0.98, 0.005, 60.0],
                [0.96, 0.008, 60.0],
                [0.92, 0.012, 60.0],
                [0.25, 0.02, 60.0],
                [0.50, 0.015, 60.0],
                [0.45, 0.05, 60.0],
                [0.35, 0.06, 60.0],
            ],
        ),
        (
            Scheme::Stone,
            Mode::Dark,
            [
                [0.12, 0.01, 60.0],
                [0.16, 0.012, 60.0],
                [0.22, 0.015, 60.0],
                [0.95, 0.01, 60.0],
                [0.61, 0.015, 60.0],
                [0.80, 0.03, 60.0],
                [0.90, 0.02, 60.0],
            ],
        ),
        (
            Scheme::Forest,
            Mode::Light,
            [
                [0.98, 0.008, 150.0],
                [0.96, 0.012, 150.0],
                [0.92, 0.018, 150.0],
                [0.25, 0.06, 150.0],
                [0.50, 0.06, 150.0],
                [0.49, 0.12, 150.0],
                [0.44, 0.14, 150.0],
            ],
        ),
        (
            Scheme::Forest,
            Mode::Dark,
            [
                [0.12, 0.015, 150.0],
                [0.16, 0.02, 150.0],
                [0.22, 0.025, 150.0],
                [0.95, 0.02, 150.0],
                [0.61, 0.04, 150.0],
                [0.70, 0.12, 150.0],
                [0.80, 0.10, 150.0],
            ],
        ),
        (
            Scheme::Sunset,
            Mode::Light,
            [
                [0.98, 0.01, 45.0],
                [0.96, 0.015, 45.0],
                [0.92, 0.02, 45.0],
                [0.30, 0.08, 45.0],
                [0.50, 0.08, 45.0],
                [0.52, 0.16, 45.0],
                [0.46, 0.18, 45.0],
            ],
        ),
        (
            Scheme::Sunset,
            Mode::Dark,
            [
                [0.12, 0.02, 45.0],
                [0.16, 0.025, 45.0],
                [0.22, 0.03, 45.0],
                [0.95, 0.025, 45.0],
                [0.61, 0.05, 45.0],
                [0.72, 0.14, 45.0],
                [0.82, 0.12, 45.0],
            ],
        ),
        (
            Scheme::Mono,
            Mode::Light,
            [
                [0.98, 0.0, 0.0],
                [0.96, 0.0, 0.0],
                [0.92, 0.0, 0.0],
                [0.20, 0.0, 0.0],
                [0.50, 0.0, 0.0],
                [0.25, 0.0, 0.0],
                [0.35, 0.0, 0.0],
            ],
        ),
        (
            Scheme::Mono,
            Mode::Dark,
            [
                [0.10, 0.0, 0.0],
                [0.15, 0.0, 0.0],
                [0.22, 0.0, 0.0],
                [0.95, 0.0, 0.0],
                [0.61, 0.0, 0.0],
                [0.85, 0.0, 0.0],
                [0.92, 0.0, 0.0],
            ],
        ),
    ];
    let document = parsed_trial("embedded:anchor-contract");
    let primitive_names = document
        .v1
        .primitives
        .colors
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        primitive_names,
        [
            "palette.accent.default",
            "palette.accent.hover",
            "palette.background.1",
            "palette.background.2",
            "palette.background.3",
            "palette.foreground.default",
            "palette.foreground.muted",
            "status.danger",
            "status.success",
            "status.warning",
            "transparent",
        ]
    );
    assert!(
        primitive_names
            .iter()
            .all(|name| !name.starts_with("ocean."))
    );
    let transparent = document.v1.primitives.colors["transparent"];
    assert_eq!(
        [
            transparent.l,
            transparent.c,
            transparent.h,
            transparent.alpha,
        ],
        [0.0, 0.0, 0.0, 0.0]
    );

    let compound_contexts = document
        .v1
        .modifiers
        .iter()
        .filter_map(|block| {
            let scheme = block.when.get(&ModifierAxis::Scheme)?;
            let mode = block.when.get(&ModifierAxis::Mode)?;
            (block.when.len() == 2).then_some((scheme.as_str(), mode.as_str()))
        })
        .collect::<BTreeSet<_>>();
    let expected_contexts = Scheme::ALL
        .into_iter()
        .flat_map(|scheme| {
            Mode::ALL
                .into_iter()
                .map(move |mode| (scheme.name(), mode.name()))
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(compound_contexts, expected_contexts);

    for (scheme, mode, roles) in expected {
        let context = DesignContext {
            scheme,
            mode,
            ..Default::default()
        };
        for (name, value) in ROLE_NAMES.into_iter().zip(roles) {
            assert_oklch(authored_colour(&document, &context, name), value);
        }
        let status = match mode {
            Mode::Light => [
                ("status.success", [0.45, 0.15, 145.0]),
                ("status.warning", [0.70, 0.15, 85.0]),
                ("status.danger", [0.52, 0.20, 25.0]),
            ],
            Mode::Dark => [
                ("status.success", [0.70, 0.15, 145.0]),
                ("status.warning", [0.80, 0.15, 85.0]),
                ("status.danger", [0.70, 0.18, 25.0]),
            ],
        };
        for (name, value) in status {
            assert_oklch(authored_colour(&document, &context, name), value);
        }
    }
}

#[test]
fn embedded_semantics_use_role_anchors_and_registered_selection_pairs() {
    let document = parsed_trial("embedded:semantic-contract");
    let authored = |name: &str, surface: &str, foreground: &str, backdrop: Option<&str>| {
        let PairSource::Authored(pair) = &document.v1.semantics.pairs[name] else {
            panic!("{name} should be authored")
        };
        assert_eq!(pair.surface, surface);
        assert_eq!(pair.foreground, foreground);
        assert_eq!(pair.backdrop.as_deref(), backdrop);
    };
    for name in ["base", "card", "popover"] {
        authored(
            name,
            "palette.background.1",
            "palette.foreground.default",
            None,
        );
    }
    authored(
        "secondary",
        "palette.background.2",
        "palette.foreground.default",
        None,
    );
    authored(
        "muted",
        "transparent",
        "palette.foreground.muted",
        Some("palette.background.1"),
    );
    authored("destructive", "status.danger", "palette.background.1", None);
    let PairSource::Derived { derive: primary } = &document.v1.semantics.pairs["primary"] else {
        panic!("primary should be derived")
    };
    assert_eq!(primary.name, "selection_pair");
    assert_eq!(primary.args.len(), 2);
    assert!(matches!(
        &primary.args[0],
        RecipeArgumentSource::Colour { value } if value == "palette.accent.default"
    ));
    assert!(matches!(
        &primary.args[1],
        RecipeArgumentSource::Colour { value } if value == "palette.background.2"
    ));

    let PairSource::Derived { derive: accent } = &document.v1.semantics.pairs["accent"] else {
        panic!("accent should be derived")
    };
    assert_eq!(accent.name, "control_pair");
    assert_eq!(accent.args.len(), 4);
    for (argument, expected) in accent.args.iter().zip([
        "palette.background.3",
        "palette.accent.default",
        "palette.background.1",
        "palette.foreground.default",
    ]) {
        assert!(matches!(
            argument,
            RecipeArgumentSource::Colour { value } if value == expected
        ));
    }
    let non_text = &document.v1.semantics.non_text;
    assert_eq!(non_text["border"].value, "palette.background.3");
    assert_eq!(non_text["input"].value, "transparent");
    assert_eq!(non_text["ring"].value, "palette.accent.default");
    assert!(
        non_text["ring"]
            .adjacent
            .iter()
            .any(|pair| pair == "accent")
    );
}

#[test]
fn shipped_button_authors_destructive_and_every_non_resting_interaction() {
    let document = parsed_trial("embedded:button-authored-coverage");
    let mapping = document
        .v1
        .families
        .button
        .as_ref()
        .expect("embedded button mapping");

    assert!(
        !mapping
            .inherit
            .variants
            .contains(&ButtonVariant::Destructive)
    );
    for interaction in [
        InteractionState::Hovered,
        InteractionState::Pressed,
        InteractionState::Disabled,
    ] {
        assert!(!mapping.inherit.interactions.contains(&interaction));
    }

    assert!(mapping.variants.iter().any(|rule| {
        rule.when.variant == Some(ButtonVariant::Destructive)
            && matches!(
                rule.set.get(&ButtonProperty::Pair),
                Some(MappingValueSource::Pair { value }) if value == "destructive"
            )
    }));

    for (interaction, derivation) in [
        (InteractionState::Pressed, "contrast_safe_toward"),
        (InteractionState::Disabled, "disabled_pair"),
    ] {
        let authored_variants = mapping
            .compound_variants
            .iter()
            .filter_map(|rule| {
                (rule.when.interaction == Some(interaction)
                    && matches!(
                        rule.set.get(&ButtonProperty::Pair),
                        Some(MappingValueSource::Derive { name, .. }) if name == derivation
                    ))
                .then_some(rule.when.variant)
                .flatten()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            authored_variants,
            ButtonVariant::ALL.into_iter().collect(),
            "{interaction:?} must be authored for every button variant"
        );
    }
    for (variant, derivation) in [
        (ButtonVariant::Default, "contrast_safe_lift"),
        (ButtonVariant::Primary, "contrast_safe_lift"),
        (ButtonVariant::Destructive, "contrast_safe_lift"),
        (ButtonVariant::Ghost, "contrast_safe_toward"),
    ] {
        assert!(mapping.compound_variants.iter().any(|rule| {
            rule.when.variant == Some(variant)
                && rule.when.interaction == Some(InteractionState::Hovered)
                && matches!(
                    rule.set.get(&ButtonProperty::Pair),
                    Some(MappingValueSource::Derive { name, .. }) if name == derivation
                )
        }));
    }
}

fn recipe_pair_binding(cell: &crate::ResolvedButtonCell) -> &str {
    cell.pair_recipe
        .as_ref()
        .expect("interaction cell must retain its recipe")
        .bindings
        .iter()
        .find_map(|binding| match binding {
            crate::RecipeBinding::Pair { name } => Some(name.as_str()),
            _ => None,
        })
        .expect("interaction recipe must bind a pair")
}

fn recipe_ratio_binding(cell: &crate::ResolvedButtonCell, name: &str) -> f64 {
    cell.pair_recipe
        .as_ref()
        .expect("interaction cell must retain its recipe")
        .bindings
        .iter()
        .find_map(|binding| match binding {
            crate::RecipeBinding::Ratio {
                name: binding_name,
                value,
            } if binding_name == name => Some(*value),
            _ => None,
        })
        .unwrap_or_else(|| panic!("interaction recipe must bind ratio `{name}`"))
}

const BUTTON_INTERACTION_DELTA_FLOOR: f64 = 0.02;

fn oklab_delta(left: crate::LinearRgba, right: crate::LinearRgba) -> f64 {
    let to_oklab = |value| {
        let (lightness, chroma, hue) = crate::colour_model::derivation::linear_srgb_to_oklch(value);
        let hue = hue.to_radians();
        [lightness, chroma * hue.cos(), chroma * hue.sin()]
    };
    let left = to_oklab(left);
    let right = to_oklab(right);
    ((left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2) + (left[2] - right[2]).powi(2))
        .sqrt()
}

fn delivered_pair_delta(left: &crate::ResolvedPair, right: &crate::ResolvedPair) -> f64 {
    oklab_delta(left.rendered_surface, right.rendered_surface).max(oklab_delta(
        left.rendered_foreground,
        right.rendered_foreground,
    ))
}

#[test]
fn shipped_fully_transparent_pair_cannot_fake_surface_toward_progress() {
    let document = parsed_trial("embedded:transparent-toward-probe");
    let colours = crate::colour::compile_colour_tokens(
        &document.v1,
        DesignContext {
            scheme: Scheme::Ocean,
            mode: Mode::Light,
            ..Default::default()
        },
    )
    .expect("Ocean / Light colour dictionary must compile")
    .value;
    let base = &colours.pairs["muted"];
    assert_eq!(base.surface.to_srgba8(), [0, 0, 0, 0]);
    assert_eq!(base.rendered_surface.to_srgba8(), [243, 250, 252, 255]);
    assert_eq!(base.rendered_foreground.to_srgba8(), [56, 107, 123, 255]);
    let recipe = crate::DerivationRecipe {
        name: "contrast_safe_toward",
        bindings: vec![
            crate::RecipeBinding::Pair {
                name: "muted".into(),
            },
            crate::RecipeBinding::Ratio {
                name: "probe.lift".into(),
                value: 0.06,
            },
        ],
        implicit_bindings: Vec::new(),
        substitutable_slot: Some(0),
        movement: crate::RecipeMovement::Surface,
        substitution_domain_constraints: &[],
        output: crate::RecipeOutput::Pair,
        text_contrast_postcondition: true,
        non_text_contrast_postcondition: false,
        opaque_input_precondition: None,
        opaque_output_invariant: false,
        substitution_policy: None,
    };
    let error =
        crate::colour_model::derivation::evaluate_pair_recipe(&recipe, &colours).unwrap_err();
    assert!(error.contains("pair `muted`"), "{error}");
    assert!(error.contains("fully transparent surface"), "{error}");
    assert!(error.contains("foreground must move instead"), "{error}");
}

#[test]
fn shipped_button_interactions_are_behaviourally_distinct_in_all_twelve_contexts() {
    let document = parsed_trial("embedded:button-behaviour-matrix");
    for scheme in Scheme::ALL {
        for mode in Mode::ALL {
            let context = DesignContext {
                scheme,
                mode,
                ..Default::default()
            };
            let result = compile_design(&document, context);
            let DesignCompileResult::Success(success) = result else {
                panic!(
                    "{} / {} did not compile: {result:#?}",
                    scheme.name(),
                    mode.name()
                )
            };
            let table = &success.candidate.tables().button;
            let pairs = &success.candidate.dictionary().colours.pairs;
            let cell = |variant, interaction| {
                table.cell(ButtonCellKey {
                    variant,
                    size: ButtonSize::Md,
                    interaction,
                    focus_visible: false,
                })
            };

            for variant in ButtonVariant::ALL {
                let resting = cell(variant, InteractionState::Resting);
                let hovered = cell(variant, InteractionState::Hovered);
                let pressed = cell(variant, InteractionState::Pressed);
                let disabled = cell(variant, InteractionState::Disabled);
                let label = format!("{} / {} / {}", scheme.name(), mode.name(), variant.name());

                for (interaction, reference) in [
                    (InteractionState::Resting, resting),
                    (InteractionState::Hovered, hovered),
                    (InteractionState::Pressed, pressed),
                    (InteractionState::Disabled, disabled),
                ] {
                    for size in ButtonSize::ALL {
                        for focus_visible in [false, true] {
                            let candidate = table.cell(ButtonCellKey {
                                variant,
                                size,
                                interaction,
                                focus_visible,
                            });
                            assert_eq!(
                                candidate.pair,
                                reference.pair,
                                "{label}: {interaction:?} colour must be invariant at size={} focus_visible={focus_visible}",
                                size.name()
                            );
                        }
                    }
                }

                for state in [resting, hovered, pressed, disabled] {
                    assert!(
                        state.pair.contrast_ratio >= 4.5,
                        "{label}: contrast {:.3}:1",
                        state.pair.contrast_ratio
                    );
                }
                let delivered_states = [resting, hovered, pressed, disabled]
                    .into_iter()
                    .map(|state| {
                        (
                            state.pair.rendered_surface.to_srgba8(),
                            state.pair.rendered_foreground.to_srgba8(),
                        )
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(
                    delivered_states.len(),
                    4,
                    "{label}: every interaction must deliver a distinct rendered pair"
                );
                if variant == ButtonVariant::Ghost {
                    assert!(
                        hovered.pair.contrast_ratio > pressed.pair.contrast_ratio,
                        "{label}: solid Ghost hover contrast must exceed pressed"
                    );
                    assert!(
                        pressed.pair.contrast_ratio > disabled.pair.contrast_ratio,
                        "{label}: solid Ghost press contrast must exceed disabled"
                    );
                    // The ordering below is only meaningful while resting Ghost really is
                    // an unfilled surface, so pin that premise rather than assume it.
                    assert_eq!(
                        resting.pair.surface_name, "transparent",
                        "{label}: resting Ghost must stay an unfilled surface"
                    );
                    assert_eq!(
                        resting.pair.backdrop_name.as_deref(),
                        Some("palette.background.1"),
                        "{label}: resting Ghost must composite over the page backdrop"
                    );
                    if mode == Mode::Light {
                        // Accepted residual: transparent fill plus a muted label makes
                        // resting Ghost deliberately weaker than its disabled solid state.
                        // Both halves of that premise are asserted so the residual cannot
                        // be silently converted into an ordinary strong label.
                        assert_eq!(
                            resting.pair.foreground_name, "palette.foreground.muted",
                            "{label}: light Ghost must rest on a muted label"
                        );
                        assert!(
                            disabled.pair.contrast_ratio > resting.pair.contrast_ratio,
                            "{label}: light Ghost disabled contrast must exceed its deliberately weakest resting state"
                        );
                    } else {
                        // Dark mode lifts the label to the default foreground, which is why
                        // rest is the strongest Ghost state here rather than the weakest.
                        assert_eq!(
                            resting.pair.foreground_name, "palette.foreground.default",
                            "{label}: dark Ghost must rest on the default label"
                        );
                        assert!(
                            resting.pair.contrast_ratio > hovered.pair.contrast_ratio,
                            "{label}: dark Ghost resting contrast must exceed hovered"
                        );
                    }
                } else {
                    assert!(
                        hovered.pair.contrast_ratio + f64::EPSILON >= resting.pair.contrast_ratio,
                        "{label}: solid hover contrast must not be lower than resting"
                    );
                    assert!(
                        resting.pair.contrast_ratio > pressed.pair.contrast_ratio,
                        "{label}: resting contrast must exceed pressed for a solid button"
                    );
                }
                let states = [
                    ("resting", resting),
                    ("hovered", hovered),
                    ("pressed", pressed),
                    ("disabled", disabled),
                ];
                for left_index in 0..states.len() {
                    for right_index in (left_index + 1)..states.len() {
                        let (left_name, left) = states[left_index];
                        let (right_name, right) = states[right_index];
                        let delta = delivered_pair_delta(&left.pair, &right.pair);
                        assert!(
                            delta >= BUTTON_INTERACTION_DELTA_FLOOR,
                            "{label}: {left_name}→{right_name} ΔE {delta:.4} is below the {BUTTON_INTERACTION_DELTA_FLOOR:.4} perceptibility floor"
                        );
                    }
                }

                let interaction_pair = match variant {
                    ButtonVariant::Default => "secondary",
                    ButtonVariant::Primary => "primary",
                    ButtonVariant::Destructive => "destructive",
                    ButtonVariant::Ghost => "accent",
                };
                assert_eq!(recipe_pair_binding(hovered), interaction_pair, "{label}");
                assert_eq!(recipe_pair_binding(pressed), interaction_pair, "{label}");
                let interaction_base = &pairs[interaction_pair];
                if variant == ButtonVariant::Ghost {
                    assert!(
                        hovered.pair.contrast_ratio < interaction_base.contrast_ratio,
                        "{label}: Ghost hover must move its fill toward the foreground"
                    );
                } else {
                    assert!(
                        hovered.pair.contrast_ratio + f64::EPSILON
                            >= interaction_base.contrast_ratio,
                        "{label}: solid hover must move away from its bound foreground"
                    );
                }
                assert!(
                    pressed.pair.contrast_ratio < interaction_base.contrast_ratio,
                    "{label}: press must move toward its bound foreground"
                );
                assert_ne!(
                    hovered.pair.rendered_surface.to_srgba8(),
                    interaction_base.rendered_surface.to_srgba8(),
                    "{label}: hover must change a delivered surface channel"
                );
                assert_ne!(
                    pressed.pair.rendered_surface.to_srgba8(),
                    interaction_base.rendered_surface.to_srgba8(),
                    "{label}: press must change a delivered surface channel"
                );

                let disabled_pair = match variant {
                    ButtonVariant::Default => "secondary",
                    ButtonVariant::Primary => "primary",
                    ButtonVariant::Destructive => "destructive",
                    ButtonVariant::Ghost => "accent",
                };
                assert_eq!(recipe_pair_binding(disabled), disabled_pair, "{label}");
                let disabled_base = &pairs[disabled_pair];
                assert!(
                    disabled.pair.contrast_ratio < disabled_base.contrast_ratio,
                    "{label}: disabled must move toward its bound pair"
                );
                assert_ne!(
                    disabled.pair.rendered_surface.to_srgba8(),
                    disabled_base.rendered_surface.to_srgba8(),
                    "{label}: visible disabled pair must move its surface"
                );
                assert_eq!(
                    disabled.pair.rendered_foreground.to_srgba8(),
                    disabled_base.rendered_foreground.to_srgba8(),
                    "{label}: visible disabled pair must retain its foreground"
                );
                let disabled_source = disabled_base.surface;
                let disabled_output = disabled.pair.surface;
                let source_chroma =
                    crate::colour_model::derivation::linear_srgb_to_oklch(disabled_source).1;
                let output_chroma =
                    crate::colour_model::derivation::linear_srgb_to_oklch(disabled_output).1;
                if source_chroma > 1e-5 {
                    let chroma_reduction =
                        recipe_ratio_binding(disabled, "disabled.chroma_reduction");
                    let minimum_retention = 1.0 - chroma_reduction;
                    let actual_retention = output_chroma / source_chroma;
                    assert!(
                        output_chroma < source_chroma,
                        "{label}: disabled must reduce chroma independently of contrast"
                    );
                    assert!(
                        actual_retention + 1e-5 >= minimum_retention,
                        "{label}: disabled chroma retention {actual_retention:.6} is below the authored floor {minimum_retention:.6}"
                    );
                } else {
                    assert!(
                        output_chroma <= 1e-5,
                        "{label}: an achromatic disabled input must remain achromatic"
                    );
                }
                assert_eq!(
                    disabled.pair.surface_name, disabled_base.surface_name,
                    "{label}"
                );
            }
            for interaction in InteractionState::ALL {
                let variants =
                    ButtonVariant::ALL.map(|variant| (variant, cell(variant, interaction)));
                let delivered_variants = variants
                    .into_iter()
                    .map(|(_, state)| {
                        let pair = &state.pair;
                        (
                            pair.rendered_surface.to_srgba8(),
                            pair.rendered_foreground.to_srgba8(),
                        )
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(
                    delivered_variants.len(),
                    ButtonVariant::ALL.len(),
                    "{} / {} / {}: every variant must deliver a distinct pair",
                    scheme.name(),
                    mode.name(),
                    interaction.name()
                );
                for left_index in 0..variants.len() {
                    for right_index in (left_index + 1)..variants.len() {
                        let (left_variant, left) = variants[left_index];
                        let (right_variant, right) = variants[right_index];
                        let delta = delivered_pair_delta(&left.pair, &right.pair);
                        assert!(
                            delta >= BUTTON_INTERACTION_DELTA_FLOOR,
                            "{} / {} / {}: {}→{} ΔE {delta:.4} is below the {BUTTON_INTERACTION_DELTA_FLOOR:.4} cross-variant perceptibility floor",
                            scheme.name(),
                            mode.name(),
                            interaction.name(),
                            left_variant.name(),
                            right_variant.name()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn ghost_and_default_never_share_a_source_pair() {
    let document = parsed_trial("embedded:ghost-source-pair-separation");
    for scheme in Scheme::ALL {
        for mode in Mode::ALL {
            let context = DesignContext {
                scheme,
                mode,
                ..Default::default()
            };
            let DesignCompileResult::Success(success) = compile_design(&document, context) else {
                panic!("{} / {} must compile", scheme.name(), mode.name())
            };
            let table = &success.candidate.tables().button;
            for interaction in InteractionState::ALL {
                let cell = |variant| {
                    table.cell(ButtonCellKey {
                        variant,
                        size: ButtonSize::Md,
                        interaction,
                        focus_visible: false,
                    })
                };
                let default = cell(ButtonVariant::Default);
                let ghost = cell(ButtonVariant::Ghost);
                assert_ne!(
                    default.pair_name,
                    ghost.pair_name,
                    "{} / {} / {}: Default and Ghost must not share a source pair",
                    scheme.name(),
                    mode.name(),
                    interaction.name()
                );
            }
        }
    }
}

#[test]
fn shipped_override_walker_and_artifact_query_agree_over_the_full_product() {
    let document = parsed_trial("embedded:walker-query-product");
    for scheme in Scheme::ALL {
        for mode in Mode::ALL {
            let context = DesignContext {
                scheme,
                mode,
                ..Default::default()
            };
            let result = compile_design(&document, context);
            let DesignCompileResult::Success(success) = result else {
                panic!(
                    "walker/query product fixture failed for {} / {}: {result:#?}",
                    scheme.name(),
                    mode.name()
                )
            };
            let candidate = &success.candidate;
            let colours = &candidate.dictionary().colours;
            for variant in ButtonVariant::ALL {
                for size in ButtonSize::ALL {
                    for interaction in InteractionState::ALL {
                        for focus_visible in [false, true] {
                            let key = ButtonCellKey {
                                variant,
                                size,
                                interaction,
                                focus_visible,
                            };
                            let cell = candidate.tables().button.cell(key);
                            for pair_name in colours.pairs.keys() {
                                let disposition = candidate.button_pair_override(key, pair_name);
                                let ring_policy_decision = cell
                                    .ring_recipe
                                    .as_ref()
                                    .and_then(|recipe| recipe.substitution_policy())
                                    .map(|policy| {
                                        policy
                                            .decision(pair_name)
                                            .expect("total ring policy classifies dictionary pair")
                                    });
                                if let Some(PairRefDecision::Excluded(expected)) =
                                    ring_policy_decision
                                {
                                    match disposition {
                                        PairOverrideDisposition::Unavailable(actual) => {
                                            assert_eq!(actual, expected)
                                        }
                                        actual => panic!(
                                            "ring policy/query disagreement for {} / {} / {} / {} / {} / focus_visible={} / pair `{pair_name}`: ring_policy={ring_policy_decision:?}, query={actual:?}",
                                            scheme.name(),
                                            mode.name(),
                                            variant.name(),
                                            size.name(),
                                            interaction.name(),
                                            focus_visible,
                                        ),
                                    }
                                    continue;
                                }
                                if let PairOverrideDisposition::Available(route) = disposition {
                                    let reexecuted_pair;
                                    let (painted_pair, ring_recipe) = match route {
                                        PairOverrideRoute::ReplaceWhole { pair, ring_recipe } => {
                                            (pair, ring_recipe)
                                        }
                                        PairOverrideRoute::Reexecute {
                                            recipe,
                                            ring_recipe,
                                            ..
                                        } => {
                                            let slot = recipe
                                                .substitution_policy()
                                                .expect("re-execution route has a policy")
                                                .slot();
                                            let mut substituted = recipe.clone();
                                            let crate::RecipeBinding::Pair { name } =
                                                &mut substituted.bindings[slot]
                                            else {
                                                panic!("substitution policy slot is not a pair")
                                            };
                                            *name = pair_name.clone();
                                            reexecuted_pair = crate::colour_model::derivation::evaluate_pair_recipe(
                                                &substituted,
                                                colours,
                                            )
                                            .expect("an available pair override must re-execute")
                                            .pair;
                                            (&reexecuted_pair, ring_recipe)
                                        }
                                    };
                                    assert_eq!(ring_recipe, cell.ring_recipe.as_ref());
                                    if let Some(recipe) = ring_recipe {
                                        let slot = recipe
                                            .substitution_policy()
                                            .expect("ring re-execution route has a policy")
                                            .slot();
                                        let mut substituted = recipe.clone();
                                        let crate::RecipeBinding::Pair { name } =
                                            &mut substituted.bindings[slot]
                                        else {
                                            panic!("ring substitution slot is not a pair")
                                        };
                                        *name = pair_name.clone();
                                        let evaluation = crate::colour_model::derivation::evaluate_non_text_recipe_against(
                                            &substituted,
                                            painted_pair,
                                        )
                                        .unwrap();
                                        crate::colour_model::derivation::verify_non_text_postcondition(
                                            &substituted,
                                            evaluation.value,
                                            painted_pair.rendered_surface,
                                        )
                                        .unwrap();
                                    }
                                }
                                let policy_decision = cell
                                    .pair_recipe
                                    .as_ref()
                                    .and_then(|recipe| recipe.substitution_policy())
                                    .map(|policy| {
                                        policy
                                            .decision(pair_name)
                                            .expect("total policy classifies dictionary pair")
                                    });
                                match (policy_decision, disposition) {
                                    (
                                        Some(PairRefDecision::Admitted),
                                        PairOverrideDisposition::Available(
                                            PairOverrideRoute::Reexecute { pair, recipe, .. },
                                        ),
                                    ) => {
                                        assert_eq!(pair, &colours.pairs[pair_name]);
                                        assert_eq!(Some(recipe), cell.pair_recipe.as_ref());
                                        let slot = recipe
                                            .substitution_policy()
                                            .expect("re-execution route has a policy")
                                            .slot();
                                        let mut substituted = recipe.clone();
                                        let crate::RecipeBinding::Pair { name } =
                                            &mut substituted.bindings[slot]
                                        else {
                                            panic!("substitution policy slot is not a pair")
                                        };
                                        *name = pair_name.clone();
                                        let evaluation = crate::colour_model::derivation::evaluate_pair_recipe(
                                            &substituted,
                                            colours,
                                        )
                                        .unwrap_or_else(|message| {
                                            panic!(
                                                "admitted walker/query combination failed for {} / {} / {} / {} / {} / focus_visible={} / pair `{pair_name}`: {message}",
                                                scheme.name(),
                                                mode.name(),
                                                variant.name(),
                                                size.name(),
                                                interaction.name(),
                                                focus_visible,
                                            )
                                        });
                                        crate::colour_model::derivation::verify_text_postcondition(
                                            &substituted,
                                            &evaluation.pair,
                                        )
                                        .unwrap_or_else(|message| {
                                            panic!(
                                                "admitted walker/query postcondition failed for {} / {} / {} / {} / {} / focus_visible={} / pair `{pair_name}`: {message}",
                                                scheme.name(),
                                                mode.name(),
                                                variant.name(),
                                                size.name(),
                                                interaction.name(),
                                                focus_visible,
                                            )
                                        });
                                    }
                                    (
                                        Some(PairRefDecision::Excluded(expected)),
                                        PairOverrideDisposition::Unavailable(actual),
                                    ) => assert_eq!(actual, expected),
                                    (
                                        None,
                                        PairOverrideDisposition::Available(
                                            PairOverrideRoute::ReplaceWhole { pair, .. },
                                        ),
                                    ) => assert_eq!(pair, &colours.pairs[pair_name]),
                                    (expected, actual) => panic!(
                                        "walker/query disagreement for {} / {} / {} / {} / {} / focus_visible={} / pair `{pair_name}`: policy={expected:?}, query={actual:?}",
                                        scheme.name(),
                                        mode.name(),
                                        variant.name(),
                                        size.name(),
                                        interaction.name(),
                                        focus_visible,
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn artifact_query_distinguishes_unknown_direct_reexecution_and_unavailable_pairs() {
    let DesignCompileResult::Success(success) = compile_design(
        &parsed_trial("embedded:override-query-dispositions"),
        DesignContext::default(),
    ) else {
        panic!("embedded override query fixture must compile")
    };
    let resting = ButtonCellKey {
        variant: ButtonVariant::Default,
        size: ButtonSize::Md,
        interaction: InteractionState::Resting,
        focus_visible: false,
    };
    assert_eq!(
        success
            .candidate
            .button_pair_override(resting, "not-a-pair"),
        PairOverrideDisposition::UnknownPair
    );
    assert!(matches!(
        success.candidate.button_pair_override(resting, "primary"),
        PairOverrideDisposition::Available(PairOverrideRoute::ReplaceWhole { .. })
    ));

    let hovered = ButtonCellKey {
        interaction: InteractionState::Hovered,
        ..resting
    };
    assert!(matches!(
        success.candidate.button_pair_override(hovered, "secondary"),
        PairOverrideDisposition::Available(PairOverrideRoute::Reexecute { .. })
    ));
    assert_eq!(
        success.candidate.button_pair_override(hovered, "muted"),
        PairOverrideDisposition::Unavailable(&crate::PairRefExclusion::OutsideRecipeDomain {
            required: crate::RecipePairDomain::NonTransparentSurface,
        })
    );
}

/// SPEC 19 §10.2 puts the obligation on the *resolver*: it MUST consult the
/// stored classification and MUST NOT reconstruct it from registry metadata.
/// The walker has the same obligation and
/// `product_walking_obeys_the_compiled_decision_after_dictionary_values_change`
/// enforces it there. Without this test the resolver half is unguarded: the
/// walker/query agreement test compares the query against the very policy the
/// query is supposed to read, so it proves the two *agree*, not that the query
/// is where the answer came from. A query rewritten to re-derive admissibility
/// from `substitution_domain_constraints` passes every other test in the crate.
///
/// The technique is the walker test's: compile the policy while `muted` is
/// transparent, then hand the query a dictionary in which `muted` is opaque.
/// A stored-classification read still refuses; a re-derivation now admits.
///
/// What this proves, exactly: no re-derivation that reads a *colour value*
/// reachable from the query's arguments can survive, because the sweep below
/// leaves no transparent alpha anywhere in them. What it does not reach: a
/// re-derivation keyed on a *name* (`pair_name == "muted"`,
/// `surface_name == "transparent"`) reads no alpha at all, so no value
/// mutation can falsify it; nor can the sweep cover a colour field added to
/// these types after this test was written. Those rest on the compiled policy
/// being the only source of the exclusion *reason*, which a name-based
/// re-derivation must still borrow from it.
#[test]
fn artifact_query_obeys_the_compiled_decision_after_dictionary_values_change() {
    let DesignCompileResult::Success(success) = compile_design(
        &parsed_trial("embedded:query-policy-authority"),
        DesignContext::default(),
    ) else {
        panic!("embedded default must compile")
    };
    let hovered = ButtonCellKey {
        variant: ButtonVariant::Default,
        size: ButtonSize::Md,
        interaction: InteractionState::Hovered,
        focus_visible: false,
    };
    let excluded = crate::PairRefExclusion::OutsideRecipeDomain {
        required: crate::RecipePairDomain::NonTransparentSurface,
    };
    assert_eq!(
        success.candidate.button_pair_override(hovered, "muted"),
        PairOverrideDisposition::Unavailable(&excluded),
        "the compiled policy must exclude the transparent pair to begin with"
    );

    // Flip *every* colour reachable from the query's two arguments to opaque,
    // not the one representation a particular re-derivation happens to
    // consult. One pair's surface alpha is stored in at least five independent
    // places — the pair's `surface`, the primitive its `surface_name` selects,
    // the `non_text` token that copies the same primitive, and a full copy of
    // the pair (plus its `border`) inside every compiled button cell that
    // resolved to it. Flipping one leaves the others reading transparent, so a
    // resolver re-deriving through any unflipped alias would still answer
    // `Unavailable` and this test would pass over a live re-derivation.
    fn make_opaque(value: &mut crate::LinearRgba) -> usize {
        if value.alpha < 1.0 {
            value.alpha = 1.0;
            1
        } else {
            0
        }
    }
    fn make_pair_opaque(pair: &mut crate::ResolvedPair) -> usize {
        let mut flipped = make_opaque(&mut pair.surface)
            + make_opaque(&mut pair.foreground)
            + make_opaque(&mut pair.rendered_surface)
            + make_opaque(&mut pair.rendered_foreground);
        if let Some(backdrop) = pair.backdrop.as_mut() {
            flipped += make_opaque(backdrop);
        }
        flipped
    }

    let mut dictionary = success.candidate.dictionary().clone();
    let mut flipped_primitives = 0;
    for primitive in dictionary.colours.primitives.values_mut() {
        flipped_primitives += make_opaque(primitive);
    }
    let mut flipped_pairs = 0;
    for pair in dictionary.colours.pairs.values_mut() {
        flipped_pairs += make_pair_opaque(pair);
    }
    let mut flipped_non_text = 0;
    for token in dictionary.colours.non_text.values_mut() {
        flipped_non_text += make_opaque(&mut token.value);
    }

    // The compiled cells carry their own copies. `ResolvedButtonTable` keeps
    // its cells private, so the table is rebuilt rather than patched; the
    // nested loop must visit keys in the same order `ButtonCellKey::index`
    // packs them, which the read-back assertion below proves it does.
    let source_table = &success.candidate.tables().button;
    let mut flipped_cells = 0;
    let mut cells = Vec::with_capacity(source_table.len());
    let mut keys = Vec::with_capacity(source_table.len());
    for variant in ButtonVariant::ALL {
        for size in ButtonSize::ALL {
            for interaction in InteractionState::ALL {
                for focus_visible in [false, true] {
                    let key = ButtonCellKey {
                        variant,
                        size,
                        interaction,
                        focus_visible,
                    };
                    let mut cell = source_table.cell(key).clone();
                    flipped_cells += make_pair_opaque(&mut cell.pair);
                    if let Some(border) = cell.border.as_mut() {
                        flipped_cells += make_opaque(border);
                    }
                    if let Some(ring) = cell.ring.as_mut() {
                        flipped_cells += make_opaque(ring);
                    }
                    keys.push(key);
                    cells.push(cell);
                }
            }
        }
    }
    let expected = cells.clone();
    let tables = crate::ResolvedTables {
        button: crate::ResolvedButtonTable::new(cells),
    };
    for (key, cell) in keys.iter().zip(&expected) {
        assert_eq!(
            tables.button.cell(*key),
            cell,
            "the rebuilt table must place every cell back under its own key"
        );
    }

    // Anti-vacuity, per storage site: each of these really did hold a
    // transparent alpha before the sweep, so a re-derivation reading any of
    // them now admits the pair. If a refactor empties one of these categories
    // the assertion fails and asks for a deliberate decision rather than
    // silently narrowing what this test proves.
    for (site, flipped) in [
        ("primitives", flipped_primitives),
        ("dictionary pairs", flipped_pairs),
        ("non-text tokens", flipped_non_text),
        ("compiled button cells", flipped_cells),
    ] {
        assert!(
            flipped > 0,
            "no transparent alpha was found in {site}, so this test no longer proves a \
             re-derivation through {site} would be caught"
        );
    }
    let recipe = source_table
        .cell(hovered)
        .pair_recipe
        .as_ref()
        .expect("the hovered cell retains a recipe");
    let slot = recipe
        .substitution_policy()
        .expect("a retained substitutable recipe carries its policy")
        .slot();
    let constraint = recipe
        .substitution_domain_constraints
        .iter()
        .find(|constraint| constraint.param_index == slot)
        .expect("the recipe constrains its substitutable slot");
    assert!(
        constraint
            .domain
            .admits_surface_alpha(dictionary.colours.pairs["muted"].surface.alpha),
        "the swept dictionary must satisfy the recipe's domain, or a re-derivation would \
         reach the same `Unavailable` answer for the wrong reason"
    );

    assert_eq!(
        crate::design_model::button_pair_override(&tables, &dictionary, hovered, "muted"),
        PairOverrideDisposition::Unavailable(&excluded),
        "the query must answer from the compiled policy, not re-derive the domain"
    );
}

#[test]
fn embedded_default_compiles_without_errors_in_all_twelve_contexts() {
    let document = parsed_trial("embedded:all-context-gate");
    let mut unsafe_web_accents = BTreeSet::new();
    let mut diagnostic_counts = BTreeMap::<&'static str, usize>::new();
    let mut contexts_checked = 0usize;
    for scheme in Scheme::ALL {
        for mode in Mode::ALL {
            let context = DesignContext {
                scheme,
                mode,
                ..Default::default()
            };
            let result = compile_design(&document, context);
            let DesignCompileResult::Success(success) = result else {
                panic!(
                    "{} / {} did not compile: {result:#?}",
                    scheme.name(),
                    mode.name()
                )
            };
            assert!(
                success.diagnostics.iter().all(|diagnostic| {
                    diagnostic.severity == crate::DiagnosticSeverity::Warning
                        && matches!(
                            diagnostic.code,
                            "non-text-contrast" | "selection-separation" | "ring-walk-distance"
                        )
                }),
                "{:#?}",
                success.diagnostics
            );
            for diagnostic in &success.diagnostics {
                *diagnostic_counts.entry(diagnostic.code).or_default() += 1;
            }
            let colours = &success.candidate.dictionary().colours;
            assert!(
                colours
                    .pairs
                    .values()
                    .all(|pair| pair.contrast_ratio >= 4.5)
            );
            let primary = &colours.pairs["primary"];
            let accent = &colours.pairs["accent"];
            assert_ne!(
                (
                    primary.rendered_surface.to_srgba8(),
                    primary.rendered_foreground.to_srgba8()
                ),
                (
                    accent.rendered_surface.to_srgba8(),
                    accent.rendered_foreground.to_srgba8()
                ),
                "Primary and Accent must deliver byte-distinct rendered pairs"
            );
            assert_eq!(
                primary.recipe.as_ref().expect("primary recipe").name,
                "selection_pair"
            );
            assert_eq!(
                accent.recipe.as_ref().expect("accent recipe").name,
                "control_pair"
            );
            let ring = colours.non_text["ring"].value;
            assert!(contrast_ratio(ring, colours.pairs["muted"].rendered_surface) >= 3.0);
            assert!(contrast_ratio(ring, accent.rendered_surface) >= 3.0);
            let web_accent_ratio = contrast_ratio(
                colours.primitives["palette.accent.default"],
                colours.primitives["palette.background.1"],
            );
            if web_accent_ratio < 4.5 {
                unsafe_web_accents.insert((scheme, mode));
            }
            contexts_checked += 1;
        }
    }
    // `--accent` is read as link text on `--bg-primary` in every context, so the
    // authored anchor must clear AA on its own — this is the anchor, not a derived
    // pair, so no derivation can rescue it. Ocean/Light, Crimson/Dark and
    // Sunset/Light used to sit below 4.5 and were pinned here as known-bad; the
    // palette now clears all twelve, so the expected set is empty and this asserts
    // no context regresses back.
    //
    // The count is load-bearing: an empty set would also be produced by a loop that
    // stopped iterating, which would read as a pass.
    assert_eq!(
        contexts_checked, 12,
        "every scheme/mode point must be checked"
    );
    assert_eq!(unsafe_web_accents, BTreeSet::new());
    assert_eq!(
        diagnostic_counts,
        BTreeMap::from([("non-text-contrast", 156), ("ring-walk-distance", 192),])
    );
}

#[test]
fn trial_default_md_resting_resolves_shipped_control_pair() {
    let artifact = compile_trial();
    let cell = artifact.tables().button.cell(ButtonCellKey {
        variant: ButtonVariant::Default,
        size: ButtonSize::Md,
        interaction: InteractionState::Resting,
        focus_visible: false,
    });
    let typography = artifact.typography().button(ButtonTypographyKey {
        variant: ButtonVariant::Default,
        size: ButtonSize::Md,
        part: ButtonPart::Label,
    });
    assert_eq!(cell.pair_name, "secondary");
    assert_eq!(rgba(cell.pair.surface), "#E9F4F7FF");
    assert_eq!(rgba(cell.pair.foreground), "#002631FF");
    assert_eq!(rgba(cell.border.unwrap()), "#D8E8EDFF");
    assert_eq!(cell.ring, None);
    assert_eq!(
        (cell.height, cell.min_width, cell.padding_x),
        (28.0, 72.0, 10.0)
    );
    assert_eq!((cell.border_width, cell.radius), (1.0, 4.0));
    assert_eq!(typography.record.font_size, 13.333);
    assert_eq!(typography.record.font_size_metric, "type.body");
    assert_eq!(cell.pair.surface_name, "palette.background.2");
    assert_eq!(cell.pair.foreground_name, "palette.foreground.default");
    let height_provenance = artifact
        .provenance()
        .value(&crate::DesignValueId::ButtonCell {
            key: ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            },
            property: crate::ButtonProperty::Height,
        })
        .expect("height provenance");
    assert_eq!(
        height_provenance.token_path,
        ["dictionary.metrics.button.height.md"]
    );
    let type_provenance = artifact
        .provenance()
        .value(&crate::DesignValueId::TypeRecord("button.md".into()))
        .expect("type-record provenance");
    assert_eq!(type_provenance.token_path, ["dictionary.metrics.type.body"]);
    let metric_provenance = artifact
        .provenance()
        .value(&crate::DesignValueId::Metric("type.body".into()))
        .expect("type-step metric provenance");
    assert_eq!(
        metric_provenance.authored_metric,
        Some(crate::AuthoredMetric::Step {
            scale: "type".into(),
            index: 1,
        })
    );
    assert_eq!(
        artifact.dictionary().metrics["type.body"],
        crate::ResolvedMetric {
            kind: crate::ResolvedMetricKind::Px,
            value: 13.333,
        }
    );
}

#[test]
fn trial_primary_md_hovered_resolves_contrast_safe_recipe() {
    let artifact = compile_trial();
    let cell = artifact.tables().button.cell(ButtonCellKey {
        variant: ButtonVariant::Primary,
        size: ButtonSize::Md,
        interaction: InteractionState::Hovered,
        focus_visible: false,
    });
    let recipe = cell.pair_recipe.as_ref().expect("hover recipe retained");
    assert_eq!(cell.pair_name, "primary");
    assert_eq!(recipe.name, "contrast_safe_lift");
    assert_eq!(recipe.substitutable_slot, Some(0));
    assert_eq!(
        recipe.bindings,
        [
            crate::RecipeBinding::Pair {
                name: "primary".into()
            },
            crate::RecipeBinding::Ratio {
                name: "lift.hover".into(),
                value: 0.06
            }
        ]
    );
    assert_eq!(rgba(cell.pair.surface), "#044758FF");
    assert_eq!(rgba(cell.pair.foreground), "#E9F4F7FF");
    assert!(cell.pair.contrast_ratio >= 4.5);
    let lift_provenance = artifact
        .provenance()
        .value(&crate::DesignValueId::Metric("lift.hover".into()))
        .expect("lift ratio provenance");
    assert_eq!(
        lift_provenance.authored_metric,
        Some(crate::AuthoredMetric::Ratio { value: 0.06 })
    );
    assert_eq!(
        artifact.dictionary().metrics["lift.hover"].kind,
        crate::ResolvedMetricKind::Ratio
    );
    // The rule hop must name the key the theme file actually authors. The
    // embedded default spells it `compoundVariants`, and the parser accepted
    // it, so a provenance path saying `compound_variants` dead-ends.
    let pair_provenance = artifact
        .provenance()
        .value(&crate::DesignValueId::ButtonCell {
            key: ButtonCellKey {
                variant: ButtonVariant::Primary,
                size: ButtonSize::Md,
                interaction: InteractionState::Hovered,
                focus_visible: false,
            },
            property: crate::ButtonProperty::Pair,
        })
        .expect("hover pair provenance");
    assert_eq!(
        pair_provenance.applied_rule,
        "design.v1.families.button.compoundVariants[0]"
    );
    assert_eq!(
        pair_provenance.value_origin_rule,
        "design.v1.families.button.compoundVariants[0]"
    );
}

#[test]
fn trial_ghost_sm_focus_visible_resolves_backdrop_ring_and_compact_typography() {
    let artifact = compile_trial();
    let cell = artifact.tables().button.cell(ButtonCellKey {
        variant: ButtonVariant::Ghost,
        size: ButtonSize::Sm,
        interaction: InteractionState::Resting,
        focus_visible: true,
    });
    let root_type = artifact.typography().button(ButtonTypographyKey {
        variant: ButtonVariant::Ghost,
        size: ButtonSize::Sm,
        part: ButtonPart::Root,
    });
    let label_type = artifact.typography().button(ButtonTypographyKey {
        variant: ButtonVariant::Ghost,
        size: ButtonSize::Sm,
        part: ButtonPart::Label,
    });
    assert_eq!(cell.pair_name, "muted");
    assert_eq!(rgba(cell.pair.surface), "#00000000");
    assert_eq!(rgba(cell.pair.rendered_surface), "#F3FAFCFF");
    assert_eq!(rgba(cell.pair.foreground), "#386B7BFF");
    assert_eq!(rgba(cell.border.unwrap()), "#00000000");
    assert_eq!(rgba(cell.ring.unwrap()), "#006F87FF");
    assert_eq!(cell.ring_recipe.as_ref().unwrap().name, "focus_ring");
    let ring_walk = cell.ring_provenance.as_ref().unwrap();
    assert_eq!(ring_walk.seed_name, "ring");
    assert_eq!(ring_walk.step_index, 0);
    assert_eq!(ring_walk.delta_l, 0.0);
    assert_eq!((cell.height, cell.min_width), (24.0, 0.0));
    assert_eq!(root_type.name, "button.sm");
    assert_eq!(label_type.name, "button.sm");
    assert_eq!(root_type.record.font_size, 11.333);
    assert_eq!(root_type, label_type);
}

#[test]
fn focus_ring_covers_every_reachable_cell_and_pins_revision_one_walks() {
    let document = parsed_trial("embedded:focus-ring-conformance");
    let mut zero = 0usize;
    let mut routine = 0usize;
    let mut above = Vec::new();
    let mut measured = 0usize;
    for scheme in Scheme::ALL {
        for mode in Mode::ALL {
            let context = DesignContext {
                scheme,
                mode,
                ..Default::default()
            };
            let DesignCompileResult::Success(success) = compile_design(&document, context) else {
                panic!("{} / {} must compile", scheme.name(), mode.name())
            };
            let candidate = &success.candidate;
            let table = &candidate.tables().button;
            for variant in ButtonVariant::ALL {
                for size in ButtonSize::ALL {
                    for interaction in InteractionState::ALL {
                        let key = ButtonCellKey {
                            variant,
                            size,
                            interaction,
                            focus_visible: true,
                        };
                        let cell = table.cell(key);
                        if interaction == InteractionState::Disabled {
                            assert!(cell.ring.is_none());
                            continue;
                        }
                        let ring = cell
                            .ring
                            .expect("reachable focus-visible cell needs a ring");
                        assert!(
                            crate::colour_model::derivation::non_text_contrast_ratio(
                                ring,
                                cell.pair.rendered_surface,
                            ) >= 3.0,
                            "{} / {} / {} / {} / {}",
                            scheme.name(),
                            mode.name(),
                            variant.name(),
                            size.name(),
                            interaction.name(),
                        );
                        let trace = cell.ring_provenance.as_ref().unwrap();
                        assert_eq!(trace.seed_name, "ring");
                        let (down, up) =
                            crate::colour_model::derivation::focus_ring_direction_totality(
                                cell.ring_recipe.as_ref().unwrap(),
                                &cell.pair,
                            )
                            .unwrap();
                        assert!(
                            down || up,
                            "{} / {} / {} / {} / {}: neither direction reaches 3:1",
                            scheme.name(),
                            mode.name(),
                            variant.name(),
                            size.name(),
                            interaction.name(),
                        );
                        let provenance = candidate
                            .provenance()
                            .value(&crate::DesignValueId::ButtonCell {
                                key,
                                property: ButtonProperty::Ring,
                            })
                            .unwrap()
                            .focus_ring
                            .as_ref()
                            .unwrap();
                        assert_eq!(provenance, trace);
                    }
                }
            }

            let sample = table.cell(ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: true,
            });
            let base_recipe = sample.ring_recipe.as_ref().unwrap();
            for (pair_name, pair) in &candidate.dictionary().colours.pairs {
                let mut recipe = base_recipe.clone();
                let crate::RecipeBinding::Pair { name } = &mut recipe.bindings[1] else {
                    panic!("focus_ring pair slot drifted")
                };
                *name = pair_name.clone();
                let (down, up) =
                    crate::colour_model::derivation::focus_ring_direction_totality(&recipe, pair)
                        .unwrap();
                assert!(
                    down || up,
                    "{} / {} / {pair_name}: neither direction reaches 3:1",
                    scheme.name(),
                    mode.name()
                );
                let evaluation = crate::colour_model::derivation::evaluate_non_text_recipe(
                    &recipe,
                    &candidate.dictionary().colours,
                )
                .unwrap();
                measured += 1;
                let trace = evaluation.provenance;
                if trace.step_index == 0 {
                    assert_eq!(trace.delta_l, 0.0);
                    zero += 1;
                } else {
                    assert!(matches!(pair_name.as_str(), "primary" | "destructive"));
                    if trace.step_index <= 300 {
                        assert!(
                            (0.057 - 1e-9..=0.30 + 1e-9).contains(&trace.delta_l),
                            "{} / {} / {pair_name}: routine delta_l {}",
                            scheme.name(),
                            mode.name(),
                            trace.delta_l
                        );
                        assert!(evaluation.warning.is_none());
                        routine += 1;
                    } else {
                        assert_eq!(
                            evaluation.warning.as_ref().unwrap().code,
                            "ring-walk-distance"
                        );
                        let warning = &evaluation.warning.as_ref().unwrap().message;
                        assert!(warning.contains("seed `ring`"));
                        assert!(warning.contains(&format!("step_index {}", trace.step_index)));
                        above.push((scheme, mode, pair_name.clone(), trace.delta_l));
                    }
                }
            }
        }
    }
    assert_eq!(measured, 96);
    assert_eq!((zero, routine, above.len()), (72, 21, 3), "{above:?}");
    for ((scheme, mode, pair, delta), expected) in above.iter().zip([
        ("ocean", "dark", "destructive", 0.342),
        ("stone", "dark", "destructive", 0.385),
        ("mono", "dark", "destructive", 0.437),
    ]) {
        assert_eq!(
            (scheme.name(), mode.name(), pair.as_str()),
            (expected.0, expected.1, expected.2)
        );
        assert!((*delta - expected.3).abs() < 1e-9);
    }
}

#[test]
fn apply_revisions_skip_failures_and_keep_the_last_known_good() {
    let first_transition = apply_compiled_design(
        None,
        compile_design(&parsed_trial("good:first"), DesignContext::default()),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(first_transition.decision, DesignApplyDecision::Replaced);
    let first = first_transition.design.expect("first compile failed");
    assert_eq!(first.revision().get(), 1);

    let mut failed_document = parsed_trial("failed:middle");
    failed_document.v1.families.button = None;
    let failed_transition = apply_compiled_design(
        Some(first),
        compile_design(&failed_document, DesignContext::default()),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1),
    );
    assert_eq!(failed_transition.decision, DesignApplyDecision::KeptCurrent);
    assert_eq!(
        failed_transition.status.outcome,
        DesignCompileOutcome::Fatal
    );
    assert_eq!(
        failed_transition.status.compiled_at,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1)
    );
    assert_eq!(
        failed_transition.status.attempted_source.as_str(),
        "failed:middle"
    );
    // The fatal transition hands the previous artifact straight back, both
    // its identity and its revision untouched.
    let survived = failed_transition.design.expect("last known good survives");
    assert_eq!(survived.source().as_str(), "good:first");
    assert_eq!(survived.revision().get(), 1);

    let second_transition = apply_compiled_design(
        Some(survived),
        compile_design(&parsed_trial("good:second"), DesignContext::default()),
        SystemTime::UNIX_EPOCH + Duration::from_secs(2),
    );
    let second = second_transition.design.expect("second compile failed");
    assert_eq!(second.revision().get(), 2);
    assert_eq!(second.source().as_str(), "good:second");
}

// A compiler bug that reported success while emitting an error diagnostic used
// to install the artifact anyway in release builds, panicking only under
// `debug_assert`. Refusing it must not depend on the build profile.
#[test]
fn a_success_carrying_an_error_diagnostic_is_refused_and_keeps_the_current_artifact() {
    let live = compile_trial();
    let mut result = compile_design(
        &parsed_trial("contradictory:success"),
        DesignContext::default(),
    );
    let crate::DesignCompileResult::Success(success) = &mut result else {
        panic!("embedded default did not compile");
    };
    success.diagnostics.push(crate::DesignDiagnostic::error(
        "injected-contradiction",
        "design.v1",
        "a successful compile must not carry an error",
    ));

    let transition = apply_compiled_design(
        Some(live),
        result,
        SystemTime::UNIX_EPOCH + Duration::from_secs(3),
    );

    assert_eq!(transition.decision, DesignApplyDecision::KeptCurrent);
    assert_eq!(transition.status.outcome, DesignCompileOutcome::Fatal);
    let survived = transition.design.expect("last known good survives");
    assert_eq!(survived.source().as_str(), "embedded:revision-1");
    assert_eq!(survived.revision().get(), 1);
}

#[test]
fn a_boot_failure_leaves_no_live_artifact() {
    let mut failed_document = parsed_trial("failed:boot");
    failed_document.v1.families.button = None;
    let transition = apply_compiled_design(
        None,
        compile_design(&failed_document, DesignContext::default()),
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(transition.decision, DesignApplyDecision::KeptCurrent);
    assert_eq!(transition.status.outcome, DesignCompileOutcome::Fatal);
    assert!(transition.design.is_none());
}
