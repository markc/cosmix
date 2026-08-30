use std::collections::{BTreeMap, BTreeSet};

use crate::colour_model::derivation::{
    NonTextRecipeEvaluation, RecipeEvaluation, evaluate_non_text_recipe_against,
    evaluate_pair_recipe, verify_non_text_postcondition, verify_text_postcondition,
};
use crate::colour_model::{LinearRgba, ResolvedColours, ResolvedPair};
use crate::diagnostic::{CompileSuccess, DesignDiagnostic};
use crate::mapping_model::{
    BUTTON_CELL_COUNT, ButtonCellKey, ButtonProperty, ButtonTypographyKey, ButtonTypographyTable,
    ResolvedButtonCell, ResolvedButtonTable, ResolvedTypeRecord, ResolvedTypographyAssignment,
};
use crate::recipe::{DerivationRecipe, REGISTRY, RecipeBinding, RecipeSignature};
use crate::recipe_compiler::{
    compile_non_text_recipe_call, compile_pair_recipe_call, compile_pair_substitution_policy,
    push_unique_error, push_unique_warning, validate_override_product,
};
use crate::source::{
    ButtonMappingSource, CoveragePolicy, DesignV1Source, MappingRuleSource, MappingSelectorSource,
    MappingValueSource, MetricSource, TaggedMetricSource, TypeRecordSource,
};
use crate::{
    AuthoredMetric, ButtonPart, ButtonSize, ButtonVariant, DesignContext, DesignProvenance,
    DesignValueId, InteractionState, ResolvedMetric, ResolvedMetricKind, ResolvedTypography,
    ValueProvenance,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MappingCompileFailure {
    pub diagnostics: Vec<DesignDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
// Compiler-only temporary values favour direct ownership; boxing the pair
// would add allocation and dereference noise to every table cell assembly.
#[allow(clippy::large_enum_variant)]
enum CompiledValue {
    Pair {
        name: String,
        pair: ResolvedPair,
        recipe: Option<DerivationRecipe>,
    },
    DerivedPair {
        recipe: DerivationRecipe,
    },
    DerivedNonText {
        recipe: DerivationRecipe,
    },
    NonText {
        name: String,
        value: LinearRgba,
    },
    Metric {
        name: String,
        metric: CompiledMetric,
    },
    Typography {
        name: String,
        record: ResolvedTypeRecord,
    },
    Null,
}

type PairRecipeEvaluationCache = Vec<(
    DerivationRecipe,
    Option<(RecipeEvaluation, DerivationRecipe)>,
)>;
type NonTextRecipeEvaluationCache = Vec<(
    DerivationRecipe,
    ResolvedPair,
    Option<DerivationRecipe>,
    Option<(NonTextRecipeEvaluation, DerivationRecipe)>,
)>;

#[derive(Default)]
struct RecipeEvaluationCaches {
    pair: PairRecipeEvaluationCache,
    non_text: NonTextRecipeEvaluationCache,
}

struct AssembledCellPair {
    name: String,
    value: ResolvedPair,
    retained_recipe: Option<DerivationRecipe>,
    own_derivation: Option<DerivationRecipe>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompiledButtonMapping {
    pub table: ResolvedButtonTable,
    pub metrics: BTreeMap<String, CompiledMetric>,
    pub scales: BTreeMap<String, CompiledScale>,
    pub typography: ResolvedTypography,
    pub provenance: DesignProvenance,
}

pub(crate) const RADIUS_SCALE_NAME: &str = "radius";
pub(crate) const RADIUS_SCALE_GENERATOR: &str = "radius_scale";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CompiledScaleOrigin {
    Authored,
    Derived {
        generator: &'static str,
        base_metric: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompiledScale {
    pub values: Vec<f64>,
    pub origin: CompiledScaleOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthoredMetricKind {
    Px,
    Step,
    Ratio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthoredMetricPosition {
    Metric,
    ScaleEntry,
}

impl AuthoredMetricKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Px => "px",
            Self::Step => "step",
            Self::Ratio => "ratio",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CompiledMetric {
    Px {
        value: f64,
    },
    Step {
        scale: String,
        index: usize,
        px: f64,
    },
    Ratio {
        value: f64,
    },
}

impl CompiledMetric {
    pub(crate) fn resolved(&self) -> ResolvedMetric {
        match self {
            Self::Px { value } | Self::Step { px: value, .. } => ResolvedMetric {
                kind: ResolvedMetricKind::Px,
                value: *value,
            },
            Self::Ratio { value } => ResolvedMetric {
                kind: ResolvedMetricKind::Ratio,
                value: *value,
            },
        }
    }

    pub(crate) fn authored(&self) -> AuthoredMetric {
        match self {
            Self::Px { value } => AuthoredMetric::Px { value: *value },
            Self::Step { scale, index, .. } => AuthoredMetric::Step {
                scale: scale.clone(),
                index: *index,
            },
            Self::Ratio { value } => AuthoredMetric::Ratio { value: *value },
        }
    }

    const fn authored_kind(&self) -> AuthoredMetricKind {
        match self {
            Self::Px { .. } => AuthoredMetricKind::Px,
            Self::Step { .. } => AuthoredMetricKind::Step,
            Self::Ratio { .. } => AuthoredMetricKind::Ratio,
        }
    }
}

#[derive(Debug)]
struct CompiledMetrics {
    values: BTreeMap<String, CompiledMetric>,
    unresolved: BTreeSet<String>,
}

#[derive(Debug)]
struct CompiledScales {
    values: BTreeMap<String, CompiledScale>,
    invalid: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuleClass {
    Variant,
    State,
    Compound,
}

#[derive(Clone, Debug)]
struct CompiledRule {
    path: String,
    selector: MappingSelectorSource,
    values: BTreeMap<ButtonProperty, CompiledValue>,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compile_button_mapping(
    source: &DesignV1Source,
    colours: &ResolvedColours,
) -> Result<CompileSuccess<ResolvedButtonTable>, MappingCompileFailure> {
    compile_button_mapping_artifacts(source, colours, DesignContext::default()).map(|success| {
        CompileSuccess {
            value: success.value.table,
            diagnostics: success.diagnostics,
        }
    })
}

pub(crate) fn compile_button_mapping_artifacts(
    source: &DesignV1Source,
    colours: &ResolvedColours,
    context: DesignContext,
) -> Result<CompileSuccess<CompiledButtonMapping>, MappingCompileFailure> {
    let Some(mapping) = source.families.button.as_ref() else {
        return Err(MappingCompileFailure {
            diagnostics: vec![DesignDiagnostic::error(
                "missing-family",
                "design.v1.families.button",
                "the button family mapping is required",
            )],
        });
    };
    let mut errors = Vec::new();
    let available_scales = authored_scale_names(source);
    let mut scales = compile_authored_metric_scales(source, &available_scales, &mut errors);
    let radius = compile_radius_scale(source, &available_scales, &mut scales, &mut errors);
    let metrics = compile_metrics(source, &available_scales, &scales, radius, &mut errors);
    let typography = compile_typography(source, &metrics, &mut errors);
    let base = compile_base(
        mapping,
        colours,
        &metrics,
        &typography,
        context.clone(),
        &mut errors,
    );
    let mut rules = Vec::new();
    compile_rule_group(
        &mapping.variants,
        RuleClass::Variant,
        "design.v1.families.button.variants",
        colours,
        &metrics,
        &typography,
        context.clone(),
        &mut rules,
        &mut errors,
    );
    compile_rule_group(
        &mapping.states,
        RuleClass::State,
        "design.v1.families.button.states",
        colours,
        &metrics,
        &typography,
        context.clone(),
        &mut rules,
        &mut errors,
    );
    compile_rule_group(
        &mapping.compound_variants,
        RuleClass::Compound,
        // Must match the authored key, which serde renames (source.rs) - this
        // is the one camelCase field in an otherwise snake_case schema, and a
        // provenance path that does not name an authored key is a dead end.
        "design.v1.families.button.compoundVariants",
        colours,
        &metrics,
        &typography,
        context,
        &mut rules,
        &mut errors,
    );

    let mut warnings = Vec::new();
    validate_coverage(mapping, &mut warnings, &mut errors);
    if !errors.is_empty() {
        warnings.extend(errors);
        return Err(MappingCompileFailure {
            diagnostics: warnings,
        });
    }

    let mut cells = Vec::with_capacity(BUTTON_CELL_COUNT);
    let mut provenance = DesignProvenance::default();
    let mut adjacency_errors = BTreeSet::new();
    // SPEC 19 §10.7 keys eager evaluation on the recipe and every resolved
    // binding. The Vec is intentionally linear: a button table has 96 cells,
    // and DerivationRecipe's resolved bindings are the input identity.
    let mut recipe_evaluations = RecipeEvaluationCaches::default();
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
                    let mut winners = base
                        .iter()
                        .map(|(property, value)| {
                            (
                                *property,
                                (
                                    0,
                                    "design.v1.families.button.base",
                                    "design.v1.families.button.base",
                                    value.clone(),
                                ),
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    for rule in rules.iter().filter(|rule| rule.selector.matches(key)) {
                        let specificity = rule.selector.specificity();
                        for (property, candidate) in &rule.values {
                            let winner = winners.get(property).expect("base is total");
                            if specificity > winner.0 {
                                let (value_origin_rule, value) =
                                    if *candidate == CompiledValue::Null {
                                        ("design.v1.families.button.base", base[property].clone())
                                    } else {
                                        (rule.path.as_str(), candidate.clone())
                                    };
                                winners.insert(
                                    *property,
                                    (specificity, rule.path.as_str(), value_origin_rule, value),
                                );
                            } else if specificity == winner.0 {
                                errors.push(DesignDiagnostic::error(
                                    "mapping-ambiguity",
                                    &rule.path,
                                    format!(
                                        "equal-specificity rules `{}` and `{}` both set `{property:?}` for {} / {} / {} / focus_visible={}",
                                        winner.1,
                                        rule.path,
                                        variant.name(),
                                        size.name(),
                                        interaction.name(),
                                        focus_visible
                                    ),
                                ));
                            }
                        }
                    }
                    let Some(cell) = assemble_cell(
                        key,
                        &winners,
                        colours,
                        &mut warnings,
                        &mut errors,
                        &mut provenance,
                        &mut recipe_evaluations,
                    ) else {
                        continue;
                    };
                    if key.focus_visible
                        && key.interaction != InteractionState::Disabled
                        && cell.ring.is_none()
                    {
                        errors.push(DesignDiagnostic::error(
                            "focus-visible-covered",
                            "design.v1.families.button",
                            format!(
                                "reachable focus-visible cell has no indicator: variant={}, size={}, interaction={}",
                                key.variant.name(),
                                key.size.name(),
                                key.interaction.name()
                            ),
                        ));
                    }
                    if cell.ring_recipe.is_none()
                        && let (Some(ring_name), Some(_)) = (&cell.ring_name, cell.ring)
                    {
                        let ring = &colours.non_text[ring_name];
                        if !ring.adjacent.contains(&cell.pair_name) {
                            let key = format!("{ring_name}:{}", cell.pair_name);
                            if adjacency_errors.insert(key) {
                                errors.push(DesignDiagnostic::error(
                                    "ring-adjacency",
                                    "design.v1.families.button",
                                    format!(
                                        "ring `{ring_name}` is used on pair `{}` without declaring it adjacent",
                                        cell.pair_name
                                    ),
                                ));
                            }
                        }
                    }
                    cells.push(cell);
                }
            }
        }
    }
    if errors.is_empty() {
        let button_typography = assemble_typography_table(&base, &rules, &mut provenance);
        Ok(CompileSuccess {
            value: CompiledButtonMapping {
                table: ResolvedButtonTable::new(cells),
                metrics: metrics.values,
                scales: scales.values,
                typography: ResolvedTypography::new(typography, button_typography),
                provenance,
            },
            diagnostics: warnings,
        })
    } else {
        warnings.extend(errors);
        Err(MappingCompileFailure {
            diagnostics: warnings,
        })
    }
}

fn compile_metrics(
    source: &DesignV1Source,
    available_scales: &BTreeSet<String>,
    scales: &CompiledScales,
    radius: Option<CompiledMetric>,
    errors: &mut Vec<DesignDiagnostic>,
) -> CompiledMetrics {
    let mut values = BTreeMap::new();
    let mut unresolved = BTreeSet::new();
    if let Some(radius) = radius {
        values.insert(RADIUS_SCALE_NAME.into(), radius);
    } else {
        unresolved.insert(RADIUS_SCALE_NAME.into());
    }
    for (name, source) in &source.primitives.metrics {
        if name == RADIUS_SCALE_NAME {
            continue;
        }
        let path = format!("design.v1.primitives.metrics.{name}");
        if let Some(metric) = compile_metric(source, &path, available_scales, scales, errors) {
            values.insert(name.clone(), metric);
        } else {
            unresolved.insert(name.clone());
        }
    }
    CompiledMetrics { values, unresolved }
}

/// Provenance and origin keys flatten a scale entry to `<name>[<index>]`, so a name
/// carrying that syntax makes the key space non-injective: scales `a` and `a[0]`
/// produce the same key, and §14.2's chain stops being a lookup. No legitimate scale
/// name needs brackets, so reject rather than escape.
pub(crate) fn scale_name_is_flattenable(name: &str) -> bool {
    !name.is_empty() && !name.contains('[') && !name.contains(']')
}

pub(crate) const SCALE_NAME_RULE: &str = "scale names must be non-empty and free of `[` and `]`";

fn compile_authored_metric_scales(
    source: &DesignV1Source,
    available_scales: &BTreeSet<String>,
    errors: &mut Vec<DesignDiagnostic>,
) -> CompiledScales {
    let mut values = BTreeMap::new();
    let mut invalid = BTreeSet::new();
    for (name, entries) in &source.primitives.scales {
        if name == RADIUS_SCALE_NAME {
            errors.push(derived_scale_authored_diagnostic(
                "design.v1.primitives.scales.radius",
            ));
            invalid.insert(name.clone());
            continue;
        }
        // The compiler entry point rejects these before any context is flattened
        // (compiler.rs validate_scale_names); this is the same guard for callers
        // that come straight to the mapping compiler.
        if !scale_name_is_flattenable(name) {
            errors.push(DesignDiagnostic::error(
                "invalid-scale-name",
                format!("design.v1.primitives.scales.{name}"),
                SCALE_NAME_RULE,
            ));
            invalid.insert(name.clone());
            continue;
        }
        let mut resolved = Vec::with_capacity(entries.len());
        let mut valid = true;
        for (index, entry) in entries.iter().enumerate() {
            let path = format!("design.v1.primitives.scales.{name}[{index}]");
            let Some((tagged, _)) = validate_authored_metric(
                entry,
                &path,
                AuthoredMetricPosition::ScaleEntry,
                available_scales,
                errors,
            ) else {
                valid = false;
                continue;
            };
            resolved.push(tagged.value);
        }
        if valid {
            values.insert(
                name.clone(),
                CompiledScale {
                    values: resolved,
                    origin: CompiledScaleOrigin::Authored,
                },
            );
        } else {
            invalid.insert(name.clone());
        }
    }
    CompiledScales { values, invalid }
}

pub(crate) fn derived_scale_authored_diagnostic(path: impl Into<String>) -> DesignDiagnostic {
    DesignDiagnostic::error(
        "derived-scale-authored",
        path,
        "scale `radius` is compiler-derived from `primitives.metrics.radius` and cannot be authored",
    )
}

fn compile_radius_scale(
    source: &DesignV1Source,
    available_scales: &BTreeSet<String>,
    scales: &mut CompiledScales,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<CompiledMetric> {
    let path = "design.v1.primitives.metrics.radius";
    let Some(source) = source.primitives.metrics.get(RADIUS_SCALE_NAME) else {
        errors.push(DesignDiagnostic::error(
            "radius-base-missing",
            path,
            "required px metric base `radius` is missing",
        ));
        scales.invalid.insert(RADIUS_SCALE_NAME.into());
        return None;
    };
    let Some((tagged, kind)) = validate_authored_metric(
        source,
        path,
        AuthoredMetricPosition::Metric,
        available_scales,
        errors,
    ) else {
        scales.invalid.insert(RADIUS_SCALE_NAME.into());
        return None;
    };
    if kind != AuthoredMetricKind::Px {
        errors.push(DesignDiagnostic::error(
            "radius-base-not-px",
            path,
            format!(
                "required metric base `radius` must be `px`, but is `{}`",
                kind.name()
            ),
        ));
        scales.invalid.insert(RADIUS_SCALE_NAME.into());
        return None;
    }

    let base = tagged.value;
    scales.values.insert(
        RADIUS_SCALE_NAME.into(),
        CompiledScale {
            values: vec![
                (base - 4.0).max(0.0),
                (base - 2.0).max(0.0),
                base,
                base + 4.0,
            ],
            origin: CompiledScaleOrigin::Derived {
                generator: RADIUS_SCALE_GENERATOR,
                base_metric: RADIUS_SCALE_NAME,
            },
        },
    );
    Some(CompiledMetric::Px { value: base })
}

fn compile_metric(
    source: &MetricSource,
    path: &str,
    available_scales: &BTreeSet<String>,
    scales: &CompiledScales,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<CompiledMetric> {
    let (tagged, kind) = validate_authored_metric(
        source,
        path,
        AuthoredMetricPosition::Metric,
        available_scales,
        errors,
    )?;
    match kind {
        AuthoredMetricKind::Px | AuthoredMetricKind::Ratio => match kind {
            AuthoredMetricKind::Px => Some(CompiledMetric::Px {
                value: tagged.value,
            }),
            AuthoredMetricKind::Ratio => Some(CompiledMetric::Ratio {
                value: tagged.value,
            }),
            AuthoredMetricKind::Step => None,
        },
        AuthoredMetricKind::Step => {
            let scale_name = tagged
                .scale
                .as_deref()
                .expect("structurally valid step metric names a scale");
            let Some(scale) = scales.values.get(scale_name) else {
                if scales.invalid.contains(scale_name) {
                    // The scale's own diagnostic already names the authored fault.
                    return None;
                }
                errors.push(DesignDiagnostic::error(
                    "metric-step-scale-unknown",
                    path,
                    format!("step metric names unknown scale `{scale_name}`"),
                ));
                return None;
            };
            let index = tagged.value as usize;
            // Range is deliberately post-flatten: a modifier replaces a scale
            // as a whole vector, so only the winning context determines its length.
            let Some(value) = scale.values.get(index).copied() else {
                errors.push(DesignDiagnostic::error(
                    "invalid-metric",
                    path,
                    format!("step index {index} is outside scale `{scale_name}`"),
                ));
                return None;
            };
            Some(CompiledMetric::Step {
                scale: scale_name.to_owned(),
                index,
                px: value,
            })
        }
    }
}

pub(crate) fn validate_authored_metric_shape(
    source: &MetricSource,
    path: &str,
    position: AuthoredMetricPosition,
    available_scales: &BTreeSet<String>,
) -> Vec<DesignDiagnostic> {
    let mut diagnostics = Vec::new();
    validate_authored_metric(source, path, position, available_scales, &mut diagnostics);
    diagnostics
}

fn validate_authored_metric<'a>(
    source: &'a MetricSource,
    path: &str,
    position: AuthoredMetricPosition,
    available_scales: &BTreeSet<String>,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<(&'a TaggedMetricSource, AuthoredMetricKind)> {
    let tagged = tagged_metric(source, path, errors)?;
    let kind = metric_kind(&tagged.kind, path, errors)?;
    if position == AuthoredMetricPosition::ScaleEntry && kind != AuthoredMetricKind::Px {
        errors.push(metric_kind_mismatch(path, AuthoredMetricKind::Px, kind));
        return None;
    }
    if !valid_metric_number(tagged.value) {
        errors.push(DesignDiagnostic::error(
            "invalid-metric",
            path,
            "metrics must have a finite non-negative value",
        ));
        return None;
    }
    match kind {
        AuthoredMetricKind::Step => {
            let Some(scale) = tagged.scale.as_deref().filter(|name| !name.is_empty()) else {
                errors.push(DesignDiagnostic::error(
                    "metric-step-scale-unknown",
                    path,
                    "step metric must name an existing scale",
                ));
                return None;
            };
            // Two independent intrinsic ceilings: at 2^53, f64 has already
            // aliased the first unrepresentable consecutive integer onto the
            // boundary, while `as usize` saturates above the target width.
            // Neither answer depends on modifier resolution.
            const MAX_EXACT_STEP_INDEX: f64 = 9_007_199_254_740_992.0;
            if tagged.value.fract() != 0.0
                || tagged.value >= MAX_EXACT_STEP_INDEX
                || tagged.value > usize::MAX as f64
            {
                errors.push(DesignDiagnostic::error(
                    "invalid-metric",
                    path,
                    "step metric value must be a non-negative integer index",
                ));
                return None;
            }
            if !available_scales.contains(scale) {
                errors.push(DesignDiagnostic::error(
                    "metric-step-scale-unknown",
                    path,
                    format!("step metric names unknown scale `{scale}`"),
                ));
                return None;
            }
        }
        AuthoredMetricKind::Px | AuthoredMetricKind::Ratio => {
            if tagged.scale.is_some() {
                errors.push(DesignDiagnostic::error(
                    "invalid-metric",
                    path,
                    format!("{} metrics cannot name a scale", kind.name()),
                ));
                return None;
            }
        }
    }
    Some((tagged, kind))
}

pub(crate) fn authored_scale_names(source: &DesignV1Source) -> BTreeSet<String> {
    source
        .primitives
        .scales
        .keys()
        .cloned()
        .chain(std::iter::once(RADIUS_SCALE_NAME.into()))
        .collect()
}

fn tagged_metric<'a>(
    source: &'a MetricSource,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<&'a TaggedMetricSource> {
    match source {
        MetricSource::Tagged(tagged) => Some(tagged),
        MetricSource::Untagged(_) => {
            errors.push(DesignDiagnostic::error(
                "metric-untagged",
                path,
                "metric values must be tagged as px, step, or ratio",
            ));
            None
        }
    }
}

fn metric_kind(
    kind: &str,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<AuthoredMetricKind> {
    let kind = match kind {
        "px" => AuthoredMetricKind::Px,
        "step" => AuthoredMetricKind::Step,
        "ratio" => AuthoredMetricKind::Ratio,
        unknown => {
            errors.push(DesignDiagnostic::error(
                "metric-kind-unknown",
                path,
                format!("unknown metric kind `{unknown}`; expected px, step, or ratio"),
            ));
            return None;
        }
    };
    Some(kind)
}

fn valid_metric_number(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn metric_kind_mismatch(
    path: &str,
    expected: AuthoredMetricKind,
    actual: AuthoredMetricKind,
) -> DesignDiagnostic {
    DesignDiagnostic::error(
        "metric-kind-mismatch",
        path,
        format!(
            "metric position expects `{}` but the referenced metric is `{}`",
            expected.name(),
            actual.name()
        ),
    )
}

fn resolve_metric_reference<'a>(
    metrics: &'a CompiledMetrics,
    name: &str,
    expected: AuthoredMetricKind,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<&'a CompiledMetric> {
    let Some(metric) = metrics.values.get(name) else {
        if metrics.unresolved.contains(name) {
            return None;
        }
        errors.push(DesignDiagnostic::error(
            "unknown-metric",
            path,
            format!("`{name}` is not a metric primitive"),
        ));
        return None;
    };
    // A step is an authored index whose resolved kind is px (§2.7), so every
    // length position accepts either direct px or a scale step. Positions that
    // specifically require a step (typography) or a ratio remain exact.
    let actual = metric.authored_kind();
    let compatible = actual == expected
        || (expected == AuthoredMetricKind::Px && actual == AuthoredMetricKind::Step);
    if !compatible {
        errors.push(metric_kind_mismatch(path, expected, metric.authored_kind()));
        return None;
    }
    Some(metric)
}

fn resolve_recipe_ratio(metrics: &CompiledMetrics, name: &str) -> Result<f64, String> {
    let Some(metric) = metrics.values.get(name) else {
        return Err(format!("`{name}` is not a resolved ratio metric"));
    };
    if metric.authored_kind() != AuthoredMetricKind::Ratio {
        return Err(format!(
            "`{name}` is `{}` but a derivation ratio argument requires `ratio`",
            metric.authored_kind().name()
        ));
    }
    Ok(metric.resolved().value)
}

fn compile_typography(
    source: &DesignV1Source,
    metrics: &CompiledMetrics,
    errors: &mut Vec<DesignDiagnostic>,
) -> BTreeMap<String, ResolvedTypeRecord> {
    source
        .typography
        .records
        .iter()
        .filter_map(|(name, record)| {
            resolve_type_record(name, record, metrics, errors).map(|record| (name.clone(), record))
        })
        .collect()
}

fn resolve_type_record(
    name: &str,
    source: &TypeRecordSource,
    metrics: &CompiledMetrics,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<ResolvedTypeRecord> {
    let path = format!("design.v1.typography.records.{name}");
    let metric = resolve_metric_reference(
        metrics,
        &source.type_step,
        AuthoredMetricKind::Step,
        &path,
        errors,
    )?;
    let font_size = metric.resolved().value;
    if source.family.trim().is_empty() || !(1..=1000).contains(&source.weight) {
        errors.push(DesignDiagnostic::error(
            "invalid-typography",
            path,
            "typography requires a non-empty family and a weight in 1..=1000",
        ));
        return None;
    }
    if source
        .line_height
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        errors.push(DesignDiagnostic::error(
            "invalid-typography",
            path,
            "line height must be finite and positive",
        ));
        return None;
    }
    Some(ResolvedTypeRecord {
        family: source.family.clone(),
        font_size_metric: source.type_step.clone(),
        font_size,
        weight: source.weight,
        line_height: source.line_height,
    })
}

fn compile_base(
    mapping: &ButtonMappingSource,
    colours: &ResolvedColours,
    metrics: &CompiledMetrics,
    typography: &BTreeMap<String, ResolvedTypeRecord>,
    context: DesignContext,
    errors: &mut Vec<DesignDiagnostic>,
) -> BTreeMap<ButtonProperty, CompiledValue> {
    let mut base = BTreeMap::new();
    for property in ButtonProperty::ALL {
        let Some(value) = mapping.base.get(&property) else {
            errors.push(DesignDiagnostic::error(
                "missing-family-base",
                "design.v1.families.button.base",
                format!("resolver-owned property `{property:?}` has no authored base"),
            ));
            continue;
        };
        if let Some(value) = compile_value(
            property,
            value,
            true,
            "design.v1.families.button.base",
            colours,
            metrics,
            typography,
            context.clone(),
            errors,
        ) {
            base.insert(property, value);
        }
    }
    base
}

#[allow(clippy::too_many_arguments)]
fn compile_rule_group(
    sources: &[MappingRuleSource],
    class: RuleClass,
    group_path: &str,
    colours: &ResolvedColours,
    metrics: &CompiledMetrics,
    typography: &BTreeMap<String, ResolvedTypeRecord>,
    context: DesignContext,
    output: &mut Vec<CompiledRule>,
    errors: &mut Vec<DesignDiagnostic>,
) {
    for (index, source) in sources.iter().enumerate() {
        let path = format!("{group_path}[{index}]");
        validate_selector(&source.when, class, &path, errors);
        if matches!(class, RuleClass::State | RuleClass::Compound)
            && source.set.contains_key(&ButtonProperty::Typography)
        {
            errors.push(DesignDiagnostic::error(
                "typography-in-state",
                &path,
                "states and compound variants cannot set typography",
            ));
        }
        let values = source
            .set
            .iter()
            .filter(|(property, _)| {
                !(matches!(class, RuleClass::State | RuleClass::Compound)
                    && **property == ButtonProperty::Typography)
            })
            .filter_map(|(property, value)| {
                compile_value(
                    *property,
                    value,
                    false,
                    &path,
                    colours,
                    metrics,
                    typography,
                    context.clone(),
                    errors,
                )
                .map(|value| (*property, value))
            })
            .collect();
        output.push(CompiledRule {
            path,
            selector: source.when.clone(),
            values,
        });
    }
}

fn validate_selector(
    selector: &MappingSelectorSource,
    class: RuleClass,
    path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) {
    if selector.checked.is_some() || selector.selected.is_some() {
        errors.push(DesignDiagnostic::error(
            "undeclared-family-axis",
            path,
            "button rules cannot select the undeclared checked or selected axes",
        ));
    }
    let variant_axes =
        usize::from(selector.variant.is_some()) + usize::from(selector.size.is_some());
    let state_axes =
        usize::from(selector.interaction.is_some()) + usize::from(selector.focus_visible.is_some());
    let valid = match class {
        RuleClass::Variant => variant_axes > 0 && state_axes == 0,
        RuleClass::State => state_axes > 0 && variant_axes == 0,
        RuleClass::Compound => variant_axes + state_axes >= 2,
    };
    if !valid {
        errors.push(DesignDiagnostic::error(
            "invalid-rule-shape",
            path,
            "rule axes do not match their variants, states, or compoundVariants section",
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_value(
    property: ButtonProperty,
    source: &MappingValueSource,
    is_base: bool,
    path: &str,
    colours: &ResolvedColours,
    metrics: &CompiledMetrics,
    typography: &BTreeMap<String, ResolvedTypeRecord>,
    context: DesignContext,
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<CompiledValue> {
    compile_value_from_registry(
        property, source, is_base, path, colours, metrics, typography, context, REGISTRY, errors,
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_value_from_registry(
    property: ButtonProperty,
    source: &MappingValueSource,
    is_base: bool,
    path: &str,
    colours: &ResolvedColours,
    metrics: &CompiledMetrics,
    typography: &BTreeMap<String, ResolvedTypeRecord>,
    context: DesignContext,
    registry: &[RecipeSignature],
    errors: &mut Vec<DesignDiagnostic>,
) -> Option<CompiledValue> {
    let invalid = |errors: &mut Vec<DesignDiagnostic>, expected: &str| {
        errors.push(DesignDiagnostic::error(
            "mapping-type",
            path,
            format!("`{property:?}` requires {expected}"),
        ));
        None
    };
    match (property, source) {
        (ButtonProperty::Pair, MappingValueSource::Pair { value }) => {
            let Some(pair) = colours.pairs.get(value) else {
                errors.push(DesignDiagnostic::error(
                    "unknown-pair",
                    path,
                    format!("`{value}` is not a semantic text pair"),
                ));
                return None;
            };
            Some(CompiledValue::Pair {
                name: value.clone(),
                pair: pair.clone(),
                recipe: pair.recipe.clone(),
            })
        }
        (ButtonProperty::Pair, MappingValueSource::Derive { name, args }) => {
            let recipe = compile_pair_recipe_call(
                name,
                args,
                context,
                colours,
                registry,
                path,
                |ratio_name| resolve_recipe_ratio(metrics, ratio_name),
                errors,
            )?;
            Some(CompiledValue::DerivedPair { recipe })
        }
        (ButtonProperty::Ring, MappingValueSource::Derive { name, args }) => {
            let recipe = compile_non_text_recipe_call(
                name,
                args,
                context,
                colours,
                registry,
                path,
                |ratio_name| resolve_recipe_ratio(metrics, ratio_name),
                errors,
            )?;
            Some(CompiledValue::DerivedNonText { recipe })
        }
        (ButtonProperty::Border | ButtonProperty::Ring, MappingValueSource::Token { value }) => {
            let legal_position = match property {
                ButtonProperty::Border => value == "border" || value == "input",
                ButtonProperty::Ring => value == "ring",
                _ => unreachable!(),
            };
            let Some(token) = colours.non_text.get(value) else {
                errors.push(DesignDiagnostic::error(
                    "unknown-non-text-token",
                    path,
                    format!("`{value}` is not a non-text colour token"),
                ));
                return None;
            };
            if !legal_position {
                errors.push(DesignDiagnostic::error(
                    "non-text-position",
                    path,
                    format!("non-text token `{value}` is not legal in `{property:?}`"),
                ));
                return None;
            }
            Some(CompiledValue::NonText {
                name: value.clone(),
                value: token.value,
            })
        }
        (
            ButtonProperty::Height
            | ButtonProperty::MinWidth
            | ButtonProperty::PaddingX
            | ButtonProperty::BorderWidth
            | ButtonProperty::Radius,
            MappingValueSource::Metric { value },
        ) => {
            let metric =
                resolve_metric_reference(metrics, value, AuthoredMetricKind::Px, path, errors)?;
            Some(CompiledValue::Metric {
                name: value.clone(),
                metric: metric.clone(),
            })
        }
        (ButtonProperty::Typography, MappingValueSource::Typography { value }) => {
            let Some(record) = typography.get(value) else {
                errors.push(DesignDiagnostic::error(
                    "unknown-typography",
                    path,
                    format!("`{value}` is not a resolved type record"),
                ));
                return None;
            };
            Some(CompiledValue::Typography {
                name: value.clone(),
                record: record.clone(),
            })
        }
        (ButtonProperty::Border | ButtonProperty::Ring, MappingValueSource::Null) if is_base => {
            Some(CompiledValue::Null)
        }
        (_, MappingValueSource::Null) if !is_base => Some(CompiledValue::Null),
        (ButtonProperty::Pair, MappingValueSource::Token { .. }) => invalid(
            errors,
            "a pair ref or a registered derivation carrying a contrast postcondition",
        ),
        _ => invalid(errors, "a value of the property's declared type"),
    }
}

fn assemble_cell(
    key: ButtonCellKey,
    winners: &BTreeMap<ButtonProperty, (usize, &str, &str, CompiledValue)>,
    colours: &ResolvedColours,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    provenance: &mut DesignProvenance,
    recipe_evaluations: &mut RecipeEvaluationCaches,
) -> Option<ResolvedButtonCell> {
    let pair = match &winners[&ButtonProperty::Pair].3 {
        CompiledValue::Pair { name, pair, recipe } => AssembledCellPair {
            name: name.clone(),
            value: pair.clone(),
            retained_recipe: recipe.clone(),
            own_derivation: None,
        },
        CompiledValue::DerivedPair { recipe } => {
            let cell_path = mapping_cell_path(winners[&ButtonProperty::Pair].1, key);
            let (evaluation, finalized_recipe) = evaluate_recipe_for_cell(
                recipe,
                colours,
                &cell_path,
                warnings,
                errors,
                &mut recipe_evaluations.pair,
            )?;
            let pair_name = recipe
                .bindings
                .iter()
                .find_map(|binding| match binding {
                    RecipeBinding::Pair { name } => Some(name.clone()),
                    _ => None,
                })
                // A primitive-only mapping derivation has no closed PairRef
                // identity. This synthetic trace name is deliberately absent
                // from `ResolvedColours::pairs`: a derived ring cannot author
                // it as its surface binding and `ring-surface-binding` must
                // reject that otherwise ambiguous composition.
                .unwrap_or_else(|| format!("derive:{}", recipe.name));
            AssembledCellPair {
                name: pair_name,
                value: evaluation.pair,
                retained_recipe: Some(finalized_recipe.clone()),
                own_derivation: Some(finalized_recipe),
            }
        }
        _ => unreachable!("validated pair base"),
    };
    let ring = match &winners[&ButtonProperty::Ring].3 {
        CompiledValue::NonText { name, value } => (Some(name.clone()), Some(*value), None, None),
        CompiledValue::DerivedNonText { recipe } => {
            let cell_path = mapping_cell_path(winners[&ButtonProperty::Ring].1, key);
            let bound_surface_name = recipe.bindings.iter().find_map(|binding| match binding {
                RecipeBinding::Pair { name } => Some(name),
                _ => None,
            });
            if bound_surface_name.map(String::as_str) != Some(pair.name.as_str()) {
                // Deduplicated because one base-authored binding fault is
                // inherited by the whole cell matrix and otherwise reports
                // itself up to 96 times, burying its own cause.
                //
                // Accepted residual: two distinct rules that produce an
                // identical message collapse to one diagnostic, whose path
                // names only the first expanded cell, so the author needs a
                // second compile to see the other site. Safe because the
                // compile stays fatal and every affected cell is still
                // dropped — nothing is silently accepted, only discovered a
                // cycle later. Reporting every site at once needs diagnostic
                // assembly to aggregate paths per cause rather than dedup at
                // the push, which is a wider change than this fault warrants.
                push_unique_error(
                    errors,
                    "ring-surface-binding",
                    &cell_path,
                    format!(
                        "derived ring binds surface pair {:?}, but the cell paints pair `{}`",
                        bound_surface_name, pair.name
                    ),
                );
                return None;
            }
            let (evaluation, finalized_recipe) = evaluate_non_text_recipe_for_cell(
                recipe,
                colours,
                &pair.value,
                pair.own_derivation.as_ref(),
                &cell_path,
                warnings,
                errors,
                &mut recipe_evaluations.non_text,
            )?;
            (
                None,
                Some(evaluation.value),
                Some(finalized_recipe),
                Some(evaluation.provenance),
            )
        }
        CompiledValue::Null => (None, None, None, None),
        _ => unreachable!("validated ring"),
    };
    for (property, winner) in winners {
        if *property == ButtonProperty::Typography {
            continue;
        }
        provenance.insert(
            DesignValueId::ButtonCell {
                key,
                property: *property,
            },
            ValueProvenance {
                token_path: compiled_token_path(&winner.3),
                applied_rule: winner.1.to_owned(),
                value_origin_rule: winner.2.to_owned(),
                authored_metric: match &winner.3 {
                    CompiledValue::Metric { metric, .. } => Some(metric.authored()),
                    _ => None,
                },
                focus_ring: (*property == ButtonProperty::Ring)
                    .then(|| ring.3.clone())
                    .flatten(),
            },
        );
    }
    let optional_colour = |property| match &winners[&property].3 {
        CompiledValue::NonText { name, value } => (Some(name.clone()), Some(*value)),
        CompiledValue::Null => (None, None),
        _ => unreachable!("validated optional colour"),
    };
    let metric = |property| match &winners[&property].3 {
        CompiledValue::Metric { metric, .. } => metric.resolved().value,
        _ => unreachable!("validated metric"),
    };
    let border = optional_colour(ButtonProperty::Border);
    Some(ResolvedButtonCell {
        pair_name: pair.name,
        pair: pair.value,
        pair_recipe: pair.retained_recipe,
        border_name: border.0,
        border: border.1,
        ring_name: ring.0,
        ring: ring.1,
        ring_recipe: ring.2,
        ring_provenance: ring.3,
        height: metric(ButtonProperty::Height),
        min_width: metric(ButtonProperty::MinWidth),
        padding_x: metric(ButtonProperty::PaddingX),
        border_width: metric(ButtonProperty::BorderWidth),
        radius: metric(ButtonProperty::Radius),
    })
}

fn evaluate_recipe_for_cell(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    cell_path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    recipe_evaluations: &mut PairRecipeEvaluationCache,
) -> Option<(RecipeEvaluation, DerivationRecipe)> {
    evaluate_recipe_for_cell_with(
        recipe,
        colours,
        cell_path,
        warnings,
        errors,
        recipe_evaluations,
        evaluate_pair_recipe,
    )
}

#[allow(clippy::too_many_arguments)]
fn evaluate_recipe_for_cell_with<F>(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    cell_path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    recipe_evaluations: &mut PairRecipeEvaluationCache,
    mut evaluate_pair: F,
) -> Option<(RecipeEvaluation, DerivationRecipe)>
where
    F: FnMut(&DerivationRecipe, &ResolvedColours) -> Result<RecipeEvaluation, String>,
{
    if let Some((_, cached)) = recipe_evaluations
        .iter()
        .find(|(cached_recipe, _)| cached_recipe == recipe)
    {
        return cached.clone();
    }

    let mut evaluation = match evaluate_pair(recipe, colours) {
        Ok(evaluation) => evaluation,
        Err(message) => {
            errors.push(DesignDiagnostic::error(
                "derivation-evaluation",
                cell_path,
                message,
            ));
            recipe_evaluations.push((recipe.clone(), None));
            return None;
        }
    };
    if let Some(warning) = evaluation.warning.as_ref() {
        push_unique_warning(warnings, warning.code, cell_path, warning.message.clone());
    }
    if let Err(message) = verify_text_postcondition(recipe, &evaluation.pair) {
        errors.push(DesignDiagnostic::error(
            "derivation-text-postcondition",
            cell_path,
            message,
        ));
        recipe_evaluations.push((recipe.clone(), None));
        return None;
    }

    let finalized_recipe = compile_pair_substitution_policy(recipe, colours, cell_path, errors)?;
    let error_count = errors.len();
    validate_override_product(
        &finalized_recipe,
        colours,
        None,
        cell_path,
        warnings,
        errors,
    );
    if errors.len() != error_count {
        recipe_evaluations.push((recipe.clone(), None));
        return None;
    }
    evaluation.pair.recipe = Some(finalized_recipe.clone());
    recipe_evaluations.push((
        recipe.clone(),
        Some((evaluation.clone(), finalized_recipe.clone())),
    ));
    Some((evaluation, finalized_recipe))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_non_text_recipe_for_cell(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    painted_pair: &ResolvedPair,
    cell_pair_derivation: Option<&DerivationRecipe>,
    cell_path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    recipe_evaluations: &mut NonTextRecipeEvaluationCache,
) -> Option<(NonTextRecipeEvaluation, DerivationRecipe)> {
    evaluate_non_text_recipe_for_cell_with(
        recipe,
        colours,
        painted_pair,
        cell_pair_derivation,
        cell_path,
        warnings,
        errors,
        recipe_evaluations,
        evaluate_non_text_recipe_against,
    )
}

#[allow(clippy::too_many_arguments)]
fn evaluate_non_text_recipe_for_cell_with<F>(
    recipe: &DerivationRecipe,
    colours: &ResolvedColours,
    painted_pair: &ResolvedPair,
    cell_pair_derivation: Option<&DerivationRecipe>,
    cell_path: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
    recipe_evaluations: &mut NonTextRecipeEvaluationCache,
    mut evaluate_non_text: F,
) -> Option<(NonTextRecipeEvaluation, DerivationRecipe)>
where
    F: FnMut(&DerivationRecipe, &ResolvedPair) -> Result<NonTextRecipeEvaluation, String>,
{
    if let Some((_, _, _, cached)) =
        recipe_evaluations
            .iter()
            .find(|(cached_recipe, cached_pair, cached_pair_derivation, _)| {
                cached_recipe == recipe
                    && cached_pair == painted_pair
                    && cached_pair_derivation.as_ref() == cell_pair_derivation
            })
    {
        return cached.clone();
    }

    let evaluation = match evaluate_non_text(recipe, painted_pair) {
        Ok(evaluation) => evaluation,
        Err(message) => {
            errors.push(DesignDiagnostic::error(
                "derivation-evaluation",
                cell_path,
                message,
            ));
            recipe_evaluations.push((
                recipe.clone(),
                painted_pair.clone(),
                cell_pair_derivation.cloned(),
                None,
            ));
            return None;
        }
    };
    if let Some(warning) = evaluation.warning.as_ref() {
        push_unique_warning(warnings, warning.code, cell_path, warning.message.clone());
    }
    if !verify_non_text_output_for_cell(recipe, evaluation.value, painted_pair, cell_path, errors) {
        recipe_evaluations.push((
            recipe.clone(),
            painted_pair.clone(),
            cell_pair_derivation.cloned(),
            None,
        ));
        return None;
    }

    let finalized_recipe = compile_pair_substitution_policy(recipe, colours, cell_path, errors)?;
    let error_count = errors.len();
    validate_override_product(
        &finalized_recipe,
        colours,
        cell_pair_derivation,
        cell_path,
        warnings,
        errors,
    );
    if errors.len() != error_count {
        recipe_evaluations.push((
            recipe.clone(),
            painted_pair.clone(),
            cell_pair_derivation.cloned(),
            None,
        ));
        return None;
    }
    recipe_evaluations.push((
        recipe.clone(),
        painted_pair.clone(),
        cell_pair_derivation.cloned(),
        Some((evaluation.clone(), finalized_recipe.clone())),
    ));
    Some((evaluation, finalized_recipe))
}

fn verify_non_text_output_for_cell(
    recipe: &DerivationRecipe,
    output: LinearRgba,
    painted_pair: &ResolvedPair,
    cell_path: &str,
    errors: &mut Vec<DesignDiagnostic>,
) -> bool {
    match verify_non_text_postcondition(recipe, output, painted_pair.rendered_surface) {
        Ok(()) => true,
        Err(message) => {
            errors.push(DesignDiagnostic::error(
                "derivation-non-text-postcondition",
                cell_path,
                message,
            ));
            false
        }
    }
}

fn mapping_cell_path(rule_path: &str, key: ButtonCellKey) -> String {
    format!(
        "{rule_path}.cell[variant={},size={},interaction={},focus_visible={}]",
        key.variant.name(),
        key.size.name(),
        key.interaction.name(),
        key.focus_visible
    )
}

fn compiled_token_path(value: &CompiledValue) -> Vec<String> {
    match value {
        CompiledValue::Pair { name, recipe, .. } => {
            let mut path = vec![format!("dictionary.colours.pairs.{name}")];
            if let Some(recipe) = recipe {
                path.extend(recipe_token_path(recipe));
            }
            path
        }
        CompiledValue::DerivedPair { recipe } | CompiledValue::DerivedNonText { recipe } => {
            recipe_token_path(recipe)
        }
        CompiledValue::NonText { name, .. } => {
            vec![format!("dictionary.colours.non_text.{name}")]
        }
        CompiledValue::Metric { name, .. } => vec![format!("dictionary.metrics.{name}")],
        CompiledValue::Typography { name, .. } => vec![format!("typography.scale.{name}")],
        CompiledValue::Null => Vec::new(),
    }
}

fn recipe_token_path(recipe: &DerivationRecipe) -> Vec<String> {
    recipe
        .bindings
        .iter()
        .flat_map(|binding| match binding {
            RecipeBinding::Pair { name } => {
                vec![format!("dictionary.colours.pairs.{name}")]
            }
            RecipeBinding::Colour { name, .. } => {
                let tier = if crate::NON_TEXT_NAMES.contains(&name.as_str()) {
                    "non_text"
                } else {
                    "primitives"
                };
                vec![format!("dictionary.colours.{tier}.{name}")]
            }
            RecipeBinding::ColourList { names, .. } => names
                .iter()
                .map(|name| format!("dictionary.colours.primitives.{name}"))
                .collect(),
            RecipeBinding::Ratio { name, .. } => {
                vec![format!("dictionary.metrics.{name}")]
            }
        })
        .collect()
}

fn assemble_typography_table(
    base: &BTreeMap<ButtonProperty, CompiledValue>,
    rules: &[CompiledRule],
    provenance: &mut DesignProvenance,
) -> ButtonTypographyTable {
    let mut assignments = Vec::with_capacity(crate::BUTTON_TYPOGRAPHY_COUNT);
    for variant in ButtonVariant::ALL {
        for size in ButtonSize::ALL {
            let cell_key = ButtonCellKey {
                variant,
                size,
                interaction: InteractionState::Resting,
                focus_visible: false,
            };
            let base_value = &base[&ButtonProperty::Typography];
            let mut winner = (
                0,
                "design.v1.families.button.base",
                "design.v1.families.button.base",
                base_value,
            );
            for rule in rules.iter().filter(|rule| rule.selector.matches(cell_key)) {
                let Some(candidate) = rule.values.get(&ButtonProperty::Typography) else {
                    continue;
                };
                let specificity = rule.selector.specificity();
                if specificity > winner.0 {
                    winner = if *candidate == CompiledValue::Null {
                        (
                            specificity,
                            rule.path.as_str(),
                            "design.v1.families.button.base",
                            base_value,
                        )
                    } else {
                        (
                            specificity,
                            rule.path.as_str(),
                            rule.path.as_str(),
                            candidate,
                        )
                    };
                }
            }
            let CompiledValue::Typography { name, .. } = winner.3 else {
                unreachable!("validated typography base")
            };
            for part in ButtonPart::ALL {
                let key = ButtonTypographyKey {
                    variant,
                    size,
                    part,
                };
                provenance.insert(
                    DesignValueId::ButtonTypography(key),
                    ValueProvenance {
                        token_path: vec![format!("typography.scale.{name}")],
                        applied_rule: winner.1.to_owned(),
                        value_origin_rule: winner.2.to_owned(),
                        authored_metric: None,
                        focus_ring: None,
                    },
                );
                assignments.push(ResolvedTypographyAssignment {
                    record_name: name.clone(),
                });
            }
        }
    }
    ButtonTypographyTable::new(assignments)
}

fn validate_coverage(
    mapping: &ButtonMappingSource,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
) {
    let selectors = mapping
        .variants
        .iter()
        .chain(&mapping.states)
        .chain(&mapping.compound_variants)
        .map(|rule| &rule.when)
        .collect::<Vec<_>>();
    for variant in ButtonVariant::ALL
        .into_iter()
        .filter(|value| *value != ButtonVariant::Default)
    {
        if !selectors
            .iter()
            .any(|selector| selector.variant == Some(variant))
            && !mapping.inherit.variants.contains(&variant)
        {
            coverage_diagnostic(
                mapping.coverage,
                "variant",
                variant.name(),
                warnings,
                errors,
            );
        }
    }
    for size in ButtonSize::ALL
        .into_iter()
        .filter(|value| *value != ButtonSize::Md)
    {
        if !selectors.iter().any(|selector| selector.size == Some(size))
            && !mapping.inherit.sizes.contains(&size)
        {
            coverage_diagnostic(mapping.coverage, "size", size.name(), warnings, errors);
        }
    }
    for interaction in InteractionState::ALL
        .into_iter()
        .filter(|value| *value != InteractionState::Resting)
    {
        if !selectors
            .iter()
            .any(|selector| selector.interaction == Some(interaction))
            && !mapping.inherit.interactions.contains(&interaction)
        {
            coverage_diagnostic(
                mapping.coverage,
                "interaction",
                interaction.name(),
                warnings,
                errors,
            );
        }
    }
    if !selectors
        .iter()
        .any(|selector| selector.focus_visible == Some(true))
        && !mapping.inherit.focus_visible
    {
        coverage_diagnostic(mapping.coverage, "focus_visible", "true", warnings, errors);
    }
}

fn coverage_diagnostic(
    coverage: CoveragePolicy,
    axis: &str,
    value: &str,
    warnings: &mut Vec<DesignDiagnostic>,
    errors: &mut Vec<DesignDiagnostic>,
) {
    let message =
        format!("button {axis} `{value}` has no authored row; add one or declare `inherit: base`");
    let diagnostic = match coverage {
        CoveragePolicy::Warn => DesignDiagnostic::warning(
            "new-variant-uncovered",
            "design.v1.families.button",
            message,
        ),
        CoveragePolicy::Explicit => DesignDiagnostic::error(
            "new-variant-uncovered",
            "design.v1.families.button",
            message,
        ),
    };
    match coverage {
        CoveragePolicy::Warn => warnings.push(diagnostic),
        CoveragePolicy::Explicit => errors.push(diagnostic),
    }
}

impl MappingSelectorSource {
    fn specificity(&self) -> usize {
        usize::from(self.variant.is_some())
            + usize::from(self.size.is_some())
            + usize::from(self.interaction.is_some())
            + usize::from(self.focus_visible.is_some())
    }

    fn matches(&self, key: ButtonCellKey) -> bool {
        self.variant.is_none_or(|value| value == key.variant)
            && self.size.is_none_or(|value| value == key.size)
            && self
                .interaction
                .is_none_or(|value| value == key.interaction)
            && self
                .focus_visible
                .is_none_or(|value| value == key.focus_visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{
        ButtonInheritanceSource, FamilyMappingsSource, PrimitiveSource, RecipeArgumentSource,
        SourceKind,
    };
    use crate::{RecipeImplicitInput, RecipeOutput, RecipeParam};

    const NO_IMPLICIT_INPUTS: &[RecipeImplicitInput] = &[];

    fn pair_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Pair {
            value: value.into(),
        }
    }

    fn ratio_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Ratio {
            value: value.into(),
        }
    }

    fn colour_arg(value: &str) -> RecipeArgumentSource {
        RecipeArgumentSource::Colour {
            value: value.into(),
        }
    }

    fn colour_list_arg(values: &[&str]) -> RecipeArgumentSource {
        RecipeArgumentSource::ColourList {
            values: values.iter().map(|value| (*value).into()).collect(),
        }
    }

    fn source_with_mapping(mapping: ButtonMappingSource) -> DesignV1Source {
        DesignV1Source {
            kind: SourceKind::Base,
            resolution_order: Vec::new(),
            modifiers: Vec::new(),
            primitives: PrimitiveSource::default(),
            semantics: Default::default(),
            typography: Default::default(),
            families: FamilyMappingsSource {
                button: Some(mapping),
            },
            v0_crosswalk: Default::default(),
        }
    }

    fn empty_colours() -> ResolvedColours {
        ResolvedColours::default()
    }

    #[test]
    fn mandatory_base_reports_each_missing_owned_property() {
        let source = source_with_mapping(ButtonMappingSource::default());
        let failure = compile_button_mapping(&source, &empty_colours()).unwrap_err();
        assert_eq!(
            failure
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == "missing-family-base")
                .count(),
            ButtonProperty::ALL.len()
        );
    }

    #[test]
    fn a_scale_name_carrying_index_syntax_is_rejected_at_this_entry_point_too() {
        // compile_design rejects these before flattening; this compiler is public in
        // its own right, so it cannot rely on that caller having run.
        let (mut source, colours) = compile_fixture(structurally_valid_mapping());
        source
            .primitives
            .scales
            .insert("type[0]".into(), vec![MetricSource::px(11.333)]);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "invalid-scale-name"),
            "{:?}",
            failure.diagnostics
        );
    }

    #[test]
    fn an_invalid_radius_base_reports_one_authored_fault_at_this_entry_point() {
        for radius in [
            MetricSource::px(-1.0),
            MetricSource::Tagged(TaggedMetricSource {
                kind: "px".into(),
                value: 6.0,
                scale: Some("type".into()),
            }),
        ] {
            let (mut source, colours) = compile_fixture(structurally_valid_mapping());
            source
                .primitives
                .metrics
                .insert(RADIUS_SCALE_NAME.into(), radius);
            let failure = compile_button_mapping(&source, &colours).unwrap_err();
            let radius_faults = failure
                .diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.code == "invalid-metric"
                        && diagnostic.path == "design.v1.primitives.metrics.radius"
                })
                .collect::<Vec<_>>();
            assert_eq!(radius_faults.len(), 1, "{:?}", failure.diagnostics);
        }
    }

    #[test]
    fn equal_specificity_is_ambiguous_even_when_values_match() {
        let mut mapping = structurally_valid_mapping();
        mapping.variants = vec![
            rule_for_variant(ButtonVariant::Primary),
            rule_for_variant(ButtonVariant::Primary),
        ];
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "mapping-ambiguity")
        );
    }

    #[test]
    fn typography_in_state_or_compound_rules_is_rejected() {
        let mut mapping = structurally_valid_mapping();
        let mut rule = MappingRuleSource {
            when: MappingSelectorSource {
                interaction: Some(InteractionState::Hovered),
                ..Default::default()
            },
            ..Default::default()
        };
        rule.set.insert(
            ButtonProperty::Typography,
            MappingValueSource::Typography {
                value: "button.md".into(),
            },
        );
        mapping.states.push(rule);
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "typography-in-state")
        );
    }

    #[test]
    fn explicit_coverage_turns_uncovered_axis_values_fatal() {
        let mut mapping = structurally_valid_mapping();
        mapping.coverage = CoveragePolicy::Explicit;
        mapping.inherit = ButtonInheritanceSource::default();
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "new-variant-uncovered")
        );
    }

    #[test]
    fn enum_indexed_table_is_total_for_every_reachable_button_cell() {
        let mapping = structurally_valid_mapping();
        let (source, colours) = compile_fixture(mapping);
        let table = compile_button_mapping(&source, &colours).unwrap().value;
        assert_eq!(table.len(), BUTTON_CELL_COUNT);
        for variant in ButtonVariant::ALL {
            for size in ButtonSize::ALL {
                for interaction in InteractionState::ALL {
                    for focus_visible in [false, true] {
                        let _ = table.cell(ButtonCellKey {
                            variant,
                            size,
                            interaction,
                            focus_visible,
                        });
                    }
                }
            }
        }
    }

    #[test]
    fn plain_lighten_is_not_accepted_in_a_text_bearing_position() {
        let mut mapping = structurally_valid_mapping();
        mapping.compound_variants.push(MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(ButtonVariant::Primary),
                interaction: Some(InteractionState::Hovered),
                ..Default::default()
            },
            set: BTreeMap::from([(
                ButtonProperty::Pair,
                MappingValueSource::Derive {
                    name: "lighten".into(),
                    args: vec![pair_arg("base"), ratio_arg("zero")],
                },
            )]),
        });
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        assert!(
            failure
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "unregistered-derivation")
        );
    }

    #[test]
    fn registered_derivation_without_text_contrast_postcondition_is_rejected_for_text() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
        let registry = [RecipeSignature {
            name: "unsafe_lift",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: false,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "unsafe_lift".into(),
                args: vec![pair_arg("base"), ratio_arg("zero")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        );
        assert_eq!(result, None);
        assert!(
            errors
                .iter()
                .any(|diagnostic| diagnostic.code
                    == "derivation-without-text-contrast-postcondition")
        );
    }

    #[test]
    fn registered_derivation_without_a_compiled_evaluator_is_refused() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
        // Passes every signature-driven check; only the evaluator is missing.
        let registry = [RecipeSignature {
            name: "tint_toward",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "tint_toward".into(),
                args: vec![pair_arg("base"), ratio_arg("zero")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        )
        .expect("signature-valid call compiles into a cell-time recipe");
        // Before the dispatch guard this silently returned a pair computed by
        // `contrast_safe_lift`, carrying a recipe named `tint_toward`.
        assert!(errors.is_empty());
        let CompiledValue::DerivedPair { recipe } = result else {
            panic!("derive call did not retain a recipe")
        };
        assert_eq!(recipe.movement, crate::RecipeMovement::Surface);
        let error = evaluate_pair_recipe(&recipe, &colours).unwrap_err();
        assert!(error.contains("registered but has no compiled evaluator"));
    }

    #[test]
    fn a_signature_whose_bindings_do_not_match_its_evaluator_is_refused() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Ratio, RecipeParam::Pair];
        // Same name as the real evaluator, arguments in the other order. This
        // shape used to reach an `unreachable!` and abort the compile.
        let registry = [RecipeSignature {
            name: "contrast_safe_lift",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(1),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "contrast_safe_lift".into(),
                args: vec![ratio_arg("zero"), pair_arg("base")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        )
        .expect("signature-valid call compiles into a cell-time recipe");
        assert!(errors.is_empty());
        let CompiledValue::DerivedPair { recipe } = result else {
            panic!("derive call did not retain a recipe")
        };
        let error = evaluate_pair_recipe(&recipe, &colours).unwrap_err();
        assert!(error.contains("bindings do not match its evaluator"));
    }

    #[test]
    fn evaluator_rejects_unexpected_implicit_bindings() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
        static IMPLICIT: &[RecipeImplicitInput] = &[RecipeImplicitInput::ContextMode];
        let registry = [RecipeSignature {
            name: "contrast_safe_lift",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: IMPLICIT,
            substitutable_slot: Some(0),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "contrast_safe_lift".into(),
                args: vec![pair_arg("base"), ratio_arg("zero")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        )
        .expect("signature-valid call retains its compiler-supplied input");
        assert!(errors.is_empty());
        let CompiledValue::DerivedPair { recipe } = result else {
            panic!("derive call did not retain a recipe")
        };
        assert_eq!(recipe.implicit_bindings.len(), 1);
        let error = evaluate_pair_recipe(&recipe, &colours).unwrap_err();
        assert!(error.contains("bindings do not match its evaluator"));
    }

    #[test]
    fn authored_argument_kind_is_checked_instead_of_inferred_from_position() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Pair, RecipeParam::Ratio];
        let registry = [RecipeSignature {
            name: "contrast_safe_lift",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: NO_IMPLICIT_INPUTS,
            substitutable_slot: Some(0),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let metrics = CompiledMetrics {
            values: BTreeMap::from([
                ("base".into(), CompiledMetric::Ratio { value: 0.0 }),
                ("zero".into(), CompiledMetric::Ratio { value: 0.0 }),
            ]),
            unresolved: BTreeSet::new(),
        };
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "contrast_safe_lift".into(),
                // `base` exists in both the pair and ratio namespaces. Only
                // the authored tag says which one the author intended.
                args: vec![ratio_arg("base"), ratio_arg("zero")],
            },
            false,
            "test",
            &colours,
            &metrics,
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        );
        assert_eq!(result, None);
        assert_eq!(
            errors
                .iter()
                .filter(|diagnostic| diagnostic.code == "derivation-argument-kind")
                .count(),
            1
        );
    }

    #[test]
    fn authored_argument_arity_is_checked_before_binding() {
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "contrast_safe_lift".into(),
                args: vec![pair_arg("base")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            REGISTRY,
            &mut errors,
        );
        assert_eq!(result, None);
        assert_eq!(
            errors
                .iter()
                .filter(|diagnostic| diagnostic.code == "derivation-arity")
                .count(),
            1
        );
    }

    #[test]
    fn state_pair_rejects_an_empty_additional_foreground_list() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_state_pair".into(),
                args: vec![
                    pair_arg("base"),
                    colour_list_arg(&[]),
                    ratio_arg("lift.zero"),
                ],
            },
        );
        let (mut source, colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift.zero".into(), MetricSource::ratio(0.0));
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let diagnostics = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-argument-cardinality")
            .collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 1, "{:?}", failure.diagnostics);
        assert!(diagnostics[0].message.contains("at least 1 colour"));
    }

    #[test]
    fn substitutable_slot_must_be_in_range_and_name_a_pair() {
        static RATIO_PARAM: &[RecipeParam] = &[RecipeParam::Ratio];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        for (slot, expected_message) in [
            (Some(1), "outside the signature's complete input list"),
            (Some(0), "is not a pair"),
        ] {
            let registry = [RecipeSignature {
                name: "malformed",
                params: RATIO_PARAM,
                cardinality_constraints: &[],
                substitution_domain_constraints: &[],
                implicit_inputs: NO_IMPLICIT_INPUTS,
                substitutable_slot: slot,
                movement: crate::RecipeMovement::None,
                output: RecipeOutput::Pair,
                text_contrast_postcondition: true,
                non_text_contrast_postcondition: false,
                opaque_input_precondition: None,
                opaque_output_invariant: false,
            }];
            let mut errors = Vec::new();
            let result = compile_value_from_registry(
                ButtonProperty::Pair,
                &MappingValueSource::Derive {
                    name: "malformed".into(),
                    args: vec![ratio_arg("zero")],
                },
                false,
                "test",
                &colours,
                &ratio_metrics(0.0),
                &BTreeMap::new(),
                DesignContext::default(),
                &registry,
                &mut errors,
            );
            assert_eq!(result, None);
            let diagnostic = errors
                .iter()
                .find(|diagnostic| diagnostic.code == "invalid-derivation-signature")
                .unwrap_or_else(|| panic!("slot {slot:?}: {errors:?}"));
            assert!(
                diagnostic.message.contains(expected_message),
                "slot {slot:?}: {diagnostic:?}"
            );
        }
    }

    #[test]
    fn substitutable_slot_cannot_point_into_the_implicit_input_region() {
        static PARAMS: &[RecipeParam] = &[RecipeParam::Pair];
        static IMPLICIT: &[RecipeImplicitInput] = &[RecipeImplicitInput::ContextMode];
        let registry = [RecipeSignature {
            name: "contrast_safe_lift",
            params: PARAMS,
            cardinality_constraints: &[],
            substitution_domain_constraints: &[],
            implicit_inputs: IMPLICIT,
            // Authored input 0 followed by implicit input 1.
            substitutable_slot: Some(1),
            movement: crate::RecipeMovement::Surface,
            output: RecipeOutput::Pair,
            text_contrast_postcondition: true,
            non_text_contrast_postcondition: false,
            opaque_input_precondition: None,
            opaque_output_invariant: false,
        }];
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let mut errors = Vec::new();
        let result = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "contrast_safe_lift".into(),
                args: vec![pair_arg("base")],
            },
            false,
            "test",
            &colours,
            &ratio_metrics(0.0),
            &BTreeMap::new(),
            DesignContext::default(),
            &registry,
            &mut errors,
        );
        assert_eq!(result, None);
        let diagnostic = errors
            .iter()
            .find(|diagnostic| diagnostic.code == "invalid-derivation-signature")
            .expect("implicit slot is rejected");
        assert!(diagnostic.message.contains("implicit input"));
    }

    #[test]
    fn generic_postcheck_verifies_the_actual_evaluator_output() {
        let grey = LinearRgba {
            red: 0.5,
            green: 0.5,
            blue: 0.5,
            alpha: 1.0,
        };
        let colours = ResolvedColours {
            primitives: BTreeMap::from([
                ("l".into(), grey),
                ("c".into(), grey),
                ("h".into(), grey),
                ("fg".into(), grey),
            ]),
            ..Default::default()
        };
        let metrics = CompiledMetrics {
            values: BTreeMap::new(),
            unresolved: BTreeSet::new(),
        };
        let mut errors = Vec::new();
        let value = compile_value_from_registry(
            ButtonProperty::Pair,
            &MappingValueSource::Derive {
                name: "control_pair".into(),
                args: vec![
                    colour_arg("l"),
                    colour_arg("c"),
                    colour_arg("h"),
                    colour_arg("fg"),
                ],
            },
            false,
            "test",
            &colours,
            &metrics,
            &BTreeMap::new(),
            DesignContext::default(),
            REGISTRY,
            &mut errors,
        )
        .expect("well-typed control recipe compiles");
        assert!(errors.is_empty());
        let CompiledValue::DerivedPair { recipe } = value else {
            panic!("control call did not retain a recipe")
        };
        let output = evaluate_pair_recipe(&recipe, &colours)
            .expect("control arithmetic is defined")
            .pair;
        assert!(output.contrast_ratio < 4.5);
        let path = "test.cell[variant=default,size=md,interaction=resting,focus_visible=false]";
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        let mut cache = Vec::new();
        let result = evaluate_recipe_for_cell_with(
            &recipe,
            &colours,
            path,
            &mut warnings,
            &mut errors,
            &mut cache,
            |_, _| {
                Ok(RecipeEvaluation {
                    pair: output.clone(),
                    warning: None,
                })
            },
        );
        assert!(
            result.is_none(),
            "invalid pair output bypassed the production text postcheck"
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "derivation-text-postcondition");
        assert_eq!(errors[0].path, path);
        assert!(errors[0].message.contains("text contrast"));
    }

    #[test]
    fn one_failing_cell_is_fatal_and_the_diagnostic_names_that_cell() {
        let mut mapping = structurally_valid_mapping();
        mapping.compound_variants.push(MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(ButtonVariant::Primary),
                size: Some(ButtonSize::Md),
                interaction: Some(InteractionState::Hovered),
                focus_visible: Some(true),
                ..Default::default()
            },
            set: BTreeMap::from([(
                ButtonProperty::Pair,
                MappingValueSource::Derive {
                    name: "contrast_safe_state_pair".into(),
                    args: vec![
                        pair_arg("base"),
                        // Black can never contrast with the fixture's black
                        // surface when the requested lift is zero.
                        colour_list_arg(&["black"]),
                        ratio_arg("lift.zero"),
                    ],
                },
            )]),
        });
        let (mut source, colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift.zero".into(), MetricSource::ratio(0.0));
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let failures = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-evaluation")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", failure.diagnostics);
        assert!(
            failures[0]
                .path
                .contains("cell[variant=primary,size=md,interaction=hovered,focus_visible=true]")
        );
    }

    #[test]
    fn toward_recipe_rejects_a_pair_with_no_aa_safe_non_zero_step() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_toward".into(),
                args: vec![pair_arg("floor"), ratio_arg("lift")],
            },
        );
        let (mut source, mut colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.03));
        let floor = resolved_pair_at_aa_floor();
        assert!(floor.contrast_ratio >= 4.5);
        colours.pairs.insert("floor".into(), floor);

        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let failures = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-evaluation")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", failure.diagnostics);
        assert!(failures[0].message.contains("pair `floor`"));
        assert!(failures[0].message.contains("ratio 0.03"));
        assert!(failures[0].message.contains("non-zero"));
    }

    #[test]
    fn identical_failing_base_recipe_is_evaluated_and_reported_once() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_state_pair".into(),
                args: vec![
                    pair_arg("base"),
                    colour_list_arg(&["black"]),
                    ratio_arg("lift.zero"),
                ],
            },
        );
        let (mut source, colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift.zero".into(), MetricSource::ratio(0.0));
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let failures = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-evaluation")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", failure.diagnostics);
        assert_eq!(
            failures[0].path,
            "design.v1.families.button.base.cell[variant=default,size=sm,interaction=resting,focus_visible=false]"
        );
    }

    #[test]
    fn failing_recipes_with_different_bound_inputs_keep_distinct_diagnostics() {
        let mut mapping = structurally_valid_mapping();
        let failing_recipe = |ratio: &str| MappingValueSource::Derive {
            name: "contrast_safe_state_pair".into(),
            args: vec![
                pair_arg("base"),
                colour_list_arg(&["black"]),
                ratio_arg(ratio),
            ],
        };
        mapping
            .base
            .insert(ButtonProperty::Pair, failing_recipe("lift.zero"));
        mapping.variants.push(MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(ButtonVariant::Primary),
                ..Default::default()
            },
            set: BTreeMap::from([(ButtonProperty::Pair, failing_recipe("lift.other"))]),
        });
        let (mut source, colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift.zero".into(), MetricSource::ratio(0.0));
        source
            .primitives
            .metrics
            .insert("lift.other".into(), MetricSource::ratio(0.1));

        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let failures = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-evaluation")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2, "{:?}", failure.diagnostics);
        assert!(failures[0].path.contains("variant=default"));
        assert!(failures[1].path.contains("variant=primary"));
    }

    #[test]
    fn mapping_recipe_override_product_domain_excludes_only_the_transparent_substitution() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_state_pair".into(),
                args: vec![
                    pair_arg("base"),
                    colour_list_arg(&["white"]),
                    ratio_arg("lift"),
                ],
            },
        );
        let (mut source, mut colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.03));
        colours.pairs.insert("base".into(), resolved_pair_at(0.20));
        colours.pairs.insert(
            "inverse".into(),
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
            },
        );

        let success = compile_button_mapping(&source, &colours).expect(
            "visible authored lift passes; only the transparent inverse substitution is outside the domain",
        );
        let recipe = success
            .value
            .cell(ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            })
            .pair_recipe
            .as_ref()
            .expect("derived cell retains its finalised recipe");
        assert_eq!(
            recipe
                .substitution_policy()
                .expect("substitutable recipe retains its policy")
                .decision("inverse"),
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

    #[test]
    fn dual_recipe_override_checks_the_surface_the_pair_recipe_would_paint() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_toward".into(),
                args: vec![pair_arg("base"), ratio_arg("lift")],
            },
        );
        mapping.base.insert(
            ButtonProperty::Ring,
            MappingValueSource::Derive {
                name: "focus_ring".into(),
                args: vec![colour_arg("black"), pair_arg("base")],
            },
        );
        let (mut source, mut colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.01));
        colours.pairs.insert("base".into(), resolved_pair_at(0.20));
        let candidate_surface =
            crate::colour_model::derivation::oklch_to_linear_srgb(0.05, 0.0, 0.0, 1.0).unwrap();
        colours.pairs.insert(
            "candidate".into(),
            ResolvedPair {
                surface_name: "candidate.surface".into(),
                surface: candidate_surface,
                foreground_name: "white".into(),
                foreground: LinearRgba::WHITE,
                backdrop_name: None,
                backdrop: None,
                rendered_surface: candidate_surface,
                rendered_foreground: LinearRgba::WHITE,
                contrast_ratio: crate::contrast_ratio(LinearRgba::WHITE, candidate_surface),
                recipe: None,
            },
        );

        let diagnostics = compile_button_mapping(&source, &colours)
            .expect("every admitted dual-recipe override must be checked and retained")
            .diagnostics;
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "ring-walk-distance"
                    && diagnostic.message.contains("step_index 466")
                    && diagnostic.message.contains("candidate.surface")
            }),
            "dual-recipe override must walk the derived painted surface: {diagnostics:#?}"
        );
    }

    #[test]
    fn dual_recipe_override_uses_the_intersection_and_pair_failures_are_not_duplicated() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_toward".into(),
                args: vec![pair_arg("base"), ratio_arg("lift")],
            },
        );
        mapping.base.insert(
            ButtonProperty::Ring,
            MappingValueSource::Derive {
                name: "focus_ring".into(),
                args: vec![colour_arg("black"), pair_arg("base")],
            },
        );
        let (mut source, mut colours) = compile_fixture(mapping);
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.03));
        colours.pairs.insert("base".into(), resolved_pair_at(0.20));
        let excluded_rendered_surface =
            crate::colour_model::derivation::oklch_to_linear_srgb(0.05, 0.0, 0.0, 1.0).unwrap();
        colours.pairs.insert(
            "transparent".into(),
            ResolvedPair {
                surface_name: "transparent.surface".into(),
                surface: LinearRgba {
                    alpha: 0.0,
                    ..LinearRgba::BLACK
                },
                foreground_name: "white".into(),
                foreground: LinearRgba::WHITE,
                backdrop_name: Some("excluded.backdrop".into()),
                backdrop: Some(excluded_rendered_surface),
                rendered_surface: excluded_rendered_surface,
                rendered_foreground: LinearRgba::WHITE,
                contrast_ratio: crate::contrast_ratio(LinearRgba::WHITE, excluded_rendered_surface),
                recipe: None,
            },
        );

        let success = compile_button_mapping(&source, &colours)
            .expect("the ring walk must skip a pair excluded by the cell's pair recipe");
        let cell = success.value.cell(ButtonCellKey {
            variant: ButtonVariant::Default,
            size: ButtonSize::Md,
            interaction: InteractionState::Resting,
            focus_visible: false,
        });
        assert_eq!(
            cell.pair_recipe
                .as_ref()
                .unwrap()
                .substitution_policy()
                .unwrap()
                .decision("transparent"),
            Some(&crate::PairRefDecision::Excluded(
                crate::PairRefExclusion::OutsideRecipeDomain {
                    required: crate::RecipePairDomain::NonTransparentSurface,
                }
            ))
        );
        assert_eq!(
            cell.ring_recipe
                .as_ref()
                .unwrap()
                .substitution_policy()
                .unwrap()
                .decision("transparent"),
            Some(&crate::PairRefDecision::Admitted)
        );
        assert!(
            success
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("transparent.surface")),
            "the ring walker evaluated a candidate excluded by the painted-pair recipe: {:#?}",
            success.diagnostics
        );

        let (mut failing_source, mut failing_colours) = compile_fixture({
            let mut mapping = structurally_valid_mapping();
            mapping.base.insert(
                ButtonProperty::Pair,
                MappingValueSource::Derive {
                    name: "contrast_safe_toward".into(),
                    args: vec![pair_arg("base"), ratio_arg("lift")],
                },
            );
            mapping.base.insert(
                ButtonProperty::Ring,
                MappingValueSource::Derive {
                    name: "focus_ring".into(),
                    args: vec![colour_arg("black"), pair_arg("base")],
                },
            );
            mapping
        });
        failing_source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.03));
        failing_colours
            .pairs
            .insert("base".into(), resolved_pair_at(0.20));
        failing_colours
            .pairs
            .insert("candidate".into(), resolved_pair_at_aa_floor());
        let failure = compile_button_mapping(&failing_source, &failing_colours)
            .expect_err("the pair recipe must own its invalid admitted substitution");
        let candidate_faults = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "override-product"
                    && diagnostic.message.contains("pair `candidate`")
            })
            .collect::<Vec<_>>();
        assert_eq!(candidate_faults.len(), 1, "{:#?}", failure.diagnostics);
        assert!(candidate_faults[0].message.contains("text-contrast"));
    }

    #[test]
    fn substitution_domain_is_observable_but_does_not_excuse_an_authored_binding() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_toward".into(),
                args: vec![pair_arg("base"), ratio_arg("lift")],
            },
        );
        let (mut source, mut colours) = compile_fixture(mapping.clone());
        source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.06));
        colours.pairs.insert(
            "muted".into(),
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
            },
        );

        let success = compile_button_mapping(&source, &colours)
            .expect("an out-of-domain hypothetical substitution must not be fatal");
        let recipe = success
            .value
            .cell(ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            })
            .pair_recipe
            .as_ref()
            .expect("derived cell retains its finalised recipe");
        assert_eq!(
            recipe
                .substitution_policy()
                .expect("substitutable recipe retains its policy")
                .decision("muted"),
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

        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "contrast_safe_toward".into(),
                args: vec![pair_arg("muted"), ratio_arg("lift")],
            },
        );
        let (mut authored_source, _) = compile_fixture(mapping.clone());
        authored_source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.06));
        let failure = compile_button_mapping(&authored_source, &colours)
            .expect_err("an authored out-of-domain binding must remain fatal");
        let authored_faults = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derivation-evaluation")
            .collect::<Vec<_>>();
        assert_eq!(authored_faults.len(), 1, "{:?}", failure.diagnostics);
        assert_eq!(
            authored_faults[0].severity,
            crate::DiagnosticSeverity::Error
        );
        assert!(authored_faults[0].message.contains("pair `muted`"));
        assert!(
            authored_faults[0]
                .message
                .contains("fully transparent surface")
        );
        assert!(
            authored_faults[0]
                .message
                .contains("foreground must move instead")
        );

        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "disabled_pair".into(),
                args: vec![pair_arg("muted"), ratio_arg("lift"), ratio_arg("chroma")],
            },
        );
        let (mut disabled_source, _) = compile_fixture(mapping);
        disabled_source
            .primitives
            .metrics
            .insert("lift".into(), MetricSource::ratio(0.06));
        disabled_source
            .primitives
            .metrics
            .insert("chroma".into(), MetricSource::ratio(0.5));
        let failure = compile_button_mapping(&disabled_source, &colours)
            .expect_err("every authored out-of-domain binding must remain fatal");
        assert!(failure.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "derivation-evaluation"
                && diagnostic.severity == crate::DiagnosticSeverity::Error
                && diagnostic.message.contains("disabled_pair")
                && diagnostic.message.contains("pair `muted`")
                && diagnostic.message.contains("outside its domain")
        }));
    }

    #[test]
    fn state_recipe_rows_execute_their_full_requested_lifts() {
        for (name, args, expected_l, slot) in [
            (
                "disabled_pair",
                vec![pair_arg("mid"), ratio_arg("lift"), ratio_arg("chroma")],
                0.33,
                Some(0),
            ),
            (
                "contrast_safe_toward",
                vec![pair_arg("mid"), ratio_arg("lift")],
                0.33,
                Some(0),
            ),
            (
                "contrast_safe_state_pair",
                vec![
                    pair_arg("mid"),
                    colour_list_arg(&["white"]),
                    ratio_arg("lift"),
                ],
                0.27,
                Some(0),
            ),
        ] {
            let mut mapping = structurally_valid_mapping();
            mapping.base.insert(
                ButtonProperty::Pair,
                MappingValueSource::Derive {
                    name: name.into(),
                    args,
                },
            );
            let (mut source, mut colours) = compile_fixture(mapping);
            source
                .primitives
                .metrics
                .insert("lift".into(), MetricSource::ratio(0.03));
            source
                .primitives
                .metrics
                .insert("chroma".into(), MetricSource::ratio(0.5));
            let mid = resolved_pair_at(0.30);
            colours.pairs.insert("base".into(), mid.clone());
            colours.pairs.insert("mid".into(), mid);
            colours
                .non_text
                .get_mut("ring")
                .unwrap()
                .adjacent
                .insert("mid".into());
            let table = compile_button_mapping(&source, &colours)
                .expect("pair recipe compiles")
                .value;
            let cell = table.cell(ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            });
            let (actual_l, _, _) =
                crate::colour_model::derivation::linear_srgb_to_oklch(cell.pair.surface);
            assert!((actual_l - expected_l).abs() < 1e-8, "{name}: {actual_l}");
            assert_eq!(cell.pair_recipe.as_ref().unwrap().substitutable_slot, slot);
            assert!(cell.pair.contrast_ratio >= 4.5);
        }
    }

    #[test]
    fn null_rule_records_applied_rule_separately_from_value_origin() {
        let mut mapping = structurally_valid_mapping();
        mapping.variants.push(MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(ButtonVariant::Primary),
                ..Default::default()
            },
            set: BTreeMap::from([(ButtonProperty::Border, MappingValueSource::Null)]),
        });
        let (source, colours) = compile_fixture(mapping);
        let compiled =
            compile_button_mapping_artifacts(&source, &colours, DesignContext::default())
                .expect("valid mapping")
                .value;
        let value = compiled
            .provenance
            .value(&DesignValueId::ButtonCell {
                key: ButtonCellKey {
                    variant: ButtonVariant::Primary,
                    size: ButtonSize::Md,
                    interaction: InteractionState::Resting,
                    focus_visible: false,
                },
                property: ButtonProperty::Border,
            })
            .expect("border provenance");
        assert_eq!(value.applied_rule, "design.v1.families.button.variants[0]");
        assert_eq!(value.value_origin_rule, "design.v1.families.button.base");
        assert_ne!(value.applied_rule, value.value_origin_rule);
    }

    fn pending_focus_recipe(seed: LinearRgba, pair_name: &str) -> DerivationRecipe {
        let signature = REGISTRY
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

    fn pending_pair_recipe(name: &'static str, pair_name: &str, lift: f64) -> DerivationRecipe {
        let signature = REGISTRY
            .iter()
            .find(|signature| signature.name == name)
            .unwrap();
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
    fn plain_reference_to_palette_derived_pair_does_not_reexecute_its_recipe() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Pair {
                value: "derived".into(),
            },
        );
        mapping.base.insert(
            ButtonProperty::Ring,
            MappingValueSource::Derive {
                name: "focus_ring".into(),
                args: vec![colour_arg("black"), pair_arg("derived")],
            },
        );
        let (source, mut colours) = compile_fixture(mapping);
        colours.pairs.insert("base".into(), resolved_pair_at(0.20));
        colours
            .pairs
            .insert("derived".into(), resolved_pair_at(0.20));
        let candidate_surface =
            crate::colour_model::derivation::oklch_to_linear_srgb(0.05, 0.0, 0.0, 1.0).unwrap();
        colours.pairs.insert(
            "candidate".into(),
            ResolvedPair {
                surface_name: "candidate.surface".into(),
                surface: candidate_surface,
                foreground_name: "white".into(),
                foreground: LinearRgba::WHITE,
                backdrop_name: None,
                backdrop: None,
                rendered_surface: candidate_surface,
                rendered_foreground: LinearRgba::WHITE,
                contrast_ratio: crate::contrast_ratio(LinearRgba::WHITE, candidate_surface),
                recipe: None,
            },
        );
        let mut policy_errors = Vec::new();
        let palette_recipe = compile_pair_substitution_policy(
            &pending_pair_recipe("contrast_safe_toward", "base", 0.01),
            &colours,
            "test.palette-derived",
            &mut policy_errors,
        )
        .unwrap();
        assert!(policy_errors.is_empty());
        colours.pairs.get_mut("derived").unwrap().recipe = Some(palette_recipe);

        let success = compile_button_mapping(&source, &colours)
            .expect("a plain reference must paint the selected dictionary pair");
        assert!(
            success.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "ring-walk-distance"
                    && diagnostic.message.contains("step_index 465")
                    && diagnostic.message.contains("candidate.surface")
            }),
            "plain pair reference must validate the ring against dictionary Q without re-executing the referenced pair's palette recipe: {:#?}",
            success.diagnostics
        );
        let tables = crate::ResolvedTables {
            button: success.value,
        };
        let dictionary = crate::ResolvedDictionary {
            colours,
            ..Default::default()
        };
        let route = crate::design_model::button_pair_override(
            &tables,
            &dictionary,
            ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            },
            "candidate",
        );
        assert!(matches!(
            route,
            crate::PairOverrideDisposition::Available(
                crate::PairOverrideRoute::ReplaceWhole { .. }
            )
        ));
    }

    #[test]
    fn primitive_only_cell_pair_derivation_cannot_bind_an_authored_ring_surface() {
        let mut mapping = structurally_valid_mapping();
        mapping.compound_variants.push(MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(ButtonVariant::Default),
                size: Some(ButtonSize::Md),
                interaction: Some(InteractionState::Resting),
                focus_visible: Some(true),
                ..Default::default()
            },
            set: BTreeMap::from([
                (
                    ButtonProperty::Pair,
                    MappingValueSource::Derive {
                        name: "control_pair".into(),
                        args: vec![
                            colour_arg("black"),
                            colour_arg("black"),
                            colour_arg("black"),
                            colour_arg("white"),
                        ],
                    },
                ),
                (
                    ButtonProperty::Ring,
                    MappingValueSource::Derive {
                        name: "focus_ring".into(),
                        args: vec![colour_arg("black"), pair_arg("base")],
                    },
                ),
            ]),
        });
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let binding_errors = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "ring-surface-binding")
            .collect::<Vec<_>>();
        assert_eq!(binding_errors.len(), 1, "{:#?}", failure.diagnostics);
        assert!(binding_errors[0].message.contains("Some(\"base\")"));
        assert!(binding_errors[0].message.contains("`derive:control_pair`"));
        assert!(
            binding_errors[0]
                .path
                .contains(".cell[variant=default,size=md,interaction=resting,focus_visible=true]")
        );
        assert!(
            failure
                .diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "focus-visible-covered"),
            "the rejected cell must be dropped before coverage: {:#?}",
            failure.diagnostics
        );
    }

    #[test]
    fn base_ring_surface_binding_fault_is_reported_once_for_all_inherited_cells() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Pair,
            MappingValueSource::Derive {
                name: "control_pair".into(),
                args: vec![
                    colour_arg("black"),
                    colour_arg("black"),
                    colour_arg("black"),
                    colour_arg("white"),
                ],
            },
        );
        mapping.base.insert(
            ButtonProperty::Ring,
            MappingValueSource::Derive {
                name: "focus_ring".into(),
                args: vec![colour_arg("black"), pair_arg("base")],
            },
        );
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let binding_errors = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "ring-surface-binding")
            .collect::<Vec<_>>();
        assert_eq!(
            binding_errors.len(),
            1,
            "one authored base fault must not expand into per-cell noise: {:#?}",
            failure.diagnostics
        );
        assert!(binding_errors[0].path.contains(".cell[variant="));
        assert!(binding_errors[0].message.contains("Some(\"base\")"));
        assert!(binding_errors[0].message.contains("`derive:control_pair`"));
    }

    #[test]
    fn focus_visible_coverage_requires_an_indicator_but_exempts_disabled() {
        let mut mapping = structurally_valid_mapping();
        mapping
            .base
            .insert(ButtonProperty::Ring, MappingValueSource::Null);
        let (source, colours) = compile_fixture(mapping);
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let coverage = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "focus-visible-covered")
            .collect::<Vec<_>>();
        assert_eq!(coverage.len(), 36);
        assert!(
            coverage
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("disabled"))
        );
    }

    #[test]
    fn translucent_focus_seed_is_rejected_at_the_resolving_cell() {
        let mut mapping = structurally_valid_mapping();
        mapping.base.insert(
            ButtonProperty::Ring,
            MappingValueSource::Derive {
                name: "focus_ring".into(),
                args: vec![colour_arg("ring"), pair_arg("base")],
            },
        );
        let (source, mut colours) = compile_fixture(mapping);
        colours.non_text.get_mut("ring").unwrap().value.alpha = 0.5;
        let failure = compile_button_mapping(&source, &colours).unwrap_err();
        let diagnostic = failure
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == "derivation-evaluation"
                    && diagnostic.message.contains("opaque seed colour `ring`")
            })
            .expect("translucent seed diagnostic");
        assert!(diagnostic.path.contains(".cell[variant="));
    }

    #[test]
    fn eager_non_text_checkers_name_the_provoking_cell() {
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let pair = &colours.pairs["base"];
        let recipe = pending_focus_recipe(LinearRgba::WHITE, "base");
        let path = "test.cell[variant=default,size=md,interaction=resting,focus_visible=true]";

        let check = |output: LinearRgba, expected: &str| {
            let mut warnings = Vec::new();
            let mut errors = Vec::new();
            let mut cache = Vec::new();
            let result = evaluate_non_text_recipe_for_cell_with(
                &recipe,
                &colours,
                pair,
                None,
                path,
                &mut warnings,
                &mut errors,
                &mut cache,
                |_, _| {
                    Ok(NonTextRecipeEvaluation {
                        value: output,
                        provenance: crate::FocusRingProvenance {
                            seed_name: "ring".into(),
                            step_index: 0,
                            delta_l: 0.0,
                        },
                        warning: None,
                    })
                },
            );
            assert!(
                result.is_none(),
                "invalid eager NonText output bypassed the production checker"
            );
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!(errors[0].path, path);
            assert!(errors[0].message.contains(expected), "{errors:?}");
        };

        check(LinearRgba::BLACK, "non-text contrast");
        check(
            LinearRgba {
                alpha: 0.5,
                ..LinearRgba::WHITE
            },
            "opaque output is invariant",
        );
    }

    fn structurally_valid_mapping() -> ButtonMappingSource {
        use ButtonProperty as P;
        let mut base = BTreeMap::new();
        base.insert(
            P::Pair,
            MappingValueSource::Pair {
                value: "base".into(),
            },
        );
        base.insert(P::Border, MappingValueSource::Null);
        base.insert(
            P::Ring,
            MappingValueSource::Token {
                value: "ring".into(),
            },
        );
        for property in [
            P::Height,
            P::MinWidth,
            P::PaddingX,
            P::BorderWidth,
            P::Radius,
        ] {
            base.insert(
                property,
                MappingValueSource::Metric {
                    value: "zero".into(),
                },
            );
        }
        base.insert(
            P::Typography,
            MappingValueSource::Typography {
                value: "button.md".into(),
            },
        );
        ButtonMappingSource {
            base,
            inherit: ButtonInheritanceSource {
                variants: vec![
                    ButtonVariant::Primary,
                    ButtonVariant::Destructive,
                    ButtonVariant::Ghost,
                ],
                sizes: vec![ButtonSize::Sm, ButtonSize::Lg],
                interactions: vec![
                    InteractionState::Hovered,
                    InteractionState::Pressed,
                    InteractionState::Disabled,
                ],
                focus_visible: true,
            },
            ..Default::default()
        }
    }

    fn rule_for_variant(variant: ButtonVariant) -> MappingRuleSource {
        MappingRuleSource {
            when: MappingSelectorSource {
                variant: Some(variant),
                ..Default::default()
            },
            set: BTreeMap::from([(
                ButtonProperty::Pair,
                MappingValueSource::Pair {
                    value: "base".into(),
                },
            )]),
        }
    }

    #[test]
    fn shared_fixture_uses_the_closed_pair_dictionary_and_coherent_ring_adjacency() {
        let (_, colours) = compile_fixture(structurally_valid_mapping());
        let expected = crate::TEXT_PAIR_NAMES
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            colours.pairs.keys().cloned().collect::<BTreeSet<_>>(),
            expected
        );
        assert_eq!(colours.non_text["ring"].adjacent, expected);
    }

    fn compile_fixture(mapping: ButtonMappingSource) -> (DesignV1Source, ResolvedColours) {
        let pairs = crate::TEXT_PAIR_NAMES
            .into_iter()
            .map(|name| {
                let mut pair = if name == "base" {
                    ResolvedPair {
                        surface_name: String::new(),
                        surface: LinearRgba::BLACK,
                        foreground_name: "white".into(),
                        foreground: LinearRgba::WHITE,
                        backdrop_name: None,
                        backdrop: None,
                        rendered_surface: LinearRgba::BLACK,
                        rendered_foreground: LinearRgba::WHITE,
                        contrast_ratio: 21.0,
                        recipe: None,
                    }
                } else {
                    resolved_pair_at(0.20)
                };
                pair.surface_name = format!("{name}.surface");
                (name.to_owned(), pair)
            })
            .collect::<BTreeMap<_, _>>();
        let mut source = source_with_mapping(mapping);
        source
            .primitives
            .metrics
            .insert("zero".into(), MetricSource::px(0.0));
        source
            .primitives
            .metrics
            .insert("radius".into(), MetricSource::px(6.0));
        source
            .primitives
            .metrics
            .insert("type.zero".into(), MetricSource::step("type", 0.0));
        source
            .primitives
            .scales
            .insert("type".into(), vec![MetricSource::px(0.0)]);
        source.typography.records.insert(
            "button.md".into(),
            TypeRecordSource {
                family: "sans".into(),
                type_step: "type.zero".into(),
                weight: 500,
                line_height: None,
            },
        );
        let colours = ResolvedColours {
            primitives: BTreeMap::from([
                ("black".into(), LinearRgba::BLACK),
                ("white".into(), LinearRgba::WHITE),
            ]),
            pairs,
            non_text: BTreeMap::from([(
                "ring".into(),
                crate::ResolvedNonTextColour {
                    value_name: "white".into(),
                    value: LinearRgba::WHITE,
                    adjacent: crate::TEXT_PAIR_NAMES
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                },
            )]),
        };
        (source, colours)
    }

    fn resolved_pair_at(lightness: f64) -> ResolvedPair {
        let surface =
            crate::colour_model::derivation::oklch_to_linear_srgb(lightness, 0.02, 220.0, 1.0)
                .unwrap();
        ResolvedPair {
            surface_name: "mid.surface".into(),
            surface,
            foreground_name: "white".into(),
            foreground: LinearRgba::WHITE,
            backdrop_name: None,
            backdrop: None,
            rendered_surface: surface,
            rendered_foreground: LinearRgba::WHITE,
            contrast_ratio: crate::contrast_ratio(LinearRgba::WHITE, surface),
            recipe: None,
        }
    }

    fn resolved_pair_at_aa_floor() -> ResolvedPair {
        let surface = LinearRgba {
            red: 0.175_000_1,
            green: 0.175_000_1,
            blue: 0.175_000_1,
            alpha: 1.0,
        };
        ResolvedPair {
            surface_name: "floor.surface".into(),
            surface,
            foreground_name: "black".into(),
            foreground: LinearRgba::BLACK,
            backdrop_name: None,
            backdrop: None,
            rendered_surface: surface,
            rendered_foreground: LinearRgba::BLACK,
            contrast_ratio: crate::contrast_ratio(surface, LinearRgba::BLACK),
            recipe: None,
        }
    }

    fn ratio_metrics(value: f64) -> CompiledMetrics {
        CompiledMetrics {
            values: BTreeMap::from([("zero".into(), CompiledMetric::Ratio { value })]),
            unresolved: BTreeSet::new(),
        }
    }
}
