use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Contrast, DesignCompileFailure, DesignCompileResult, DesignCompileSuccess, DesignContext,
    DesignDiagnostic, DesignProvenance, DesignSourceDocument, DesignV1Source, DesignValueId,
    DiagnosticSeverity, Mode, ModifierAxis, ModifierBlockSource, ResolvedDictionary,
    ResolvedTables, Scheme, SourceIdentity, UnstampedResolvedDesign, ValueProvenance,
    compile_colour_tokens,
};

/// Compile a parsed source into an unstamped candidate for `context`.
/// Revision assignment is intentionally deferred to
/// [`crate::apply_compiled_design`]. Every context reachable through the
/// source's claimed axes is compiled before the requested artifact is returned.
pub fn compile_design(
    document: &DesignSourceDocument,
    context: DesignContext,
) -> DesignCompileResult {
    let (equivalence_context, mut diagnostics) =
        crate::equivalence::validate_crosswalk_shape(document);
    diagnostics.extend(validate_modifiers(&document.v1));
    // Before anything is flattened: origin keys are built from these names, so a name
    // that collides with the `<name>[<index>]` entry syntax would corrupt the very
    // records the later guard reports through, repointing its own diagnostic at an
    // unrelated modifier.
    diagnostics.extend(validate_scale_names(&document.v1));
    diagnostics.extend(validate_authored_metrics(&document.v1));
    if has_errors(&diagnostics) {
        deduplicate_diagnostics(&mut diagnostics);
        return fatal(document, diagnostics);
    }

    let equivalence_context =
        equivalence_context.expect("a structurally complete v0 subset has scheme and mode");
    let equivalence_source = flatten_source(&document.v1, equivalence_context.clone());
    match compile_flat_source(
        &document.identity,
        &equivalence_source,
        equivalence_context.clone(),
    ) {
        DesignCompileResult::Success(success) => {
            diagnostics.extend(crate::equivalence::validate_equivalence(
                document,
                &equivalence_context,
                &success.candidate,
            ))
        }
        DesignCompileResult::Fatal(mut failure) => {
            for diagnostic in &mut failure.diagnostics {
                equivalence_source.origins.repoint_diagnostic(diagnostic);
            }
            diagnostics.extend(failure.diagnostics);
        }
    }
    if has_errors(&diagnostics) {
        deduplicate_diagnostics(&mut diagnostics);
        return fatal(document, diagnostics);
    }

    let axes = claimed_axes(&document.v1.resolution_order);
    let reachable = reachable_contexts(context.clone(), &axes, &document.v1);
    let reachable_count = reachable.len();
    let mut requested_candidate = None;
    let mut contextual_diagnostics = Vec::new();
    for reachable in reachable {
        let flattened = flatten_source(&document.v1, reachable.clone());
        let result = compile_flat_source(&document.identity, &flattened, reachable.clone());
        let is_requested = same_claimed_context(&reachable, &context, &axes);
        let mut point_diagnostics = match result {
            DesignCompileResult::Success(success) => {
                if is_requested {
                    requested_candidate = Some(success.candidate);
                }
                success.diagnostics
            }
            DesignCompileResult::Fatal(failure) => failure.diagnostics,
        };
        for diagnostic in &mut point_diagnostics {
            flattened.origins.repoint_diagnostic(diagnostic);
        }
        deduplicate_diagnostics(&mut point_diagnostics);
        contextual_diagnostics.extend(
            point_diagnostics
                .into_iter()
                .map(|diagnostic| (diagnostic, reachable.clone())),
        );
    }

    diagnostics.extend(contextualize_diagnostics(
        contextual_diagnostics,
        &axes,
        reachable_count,
    ));
    finish_compile(document, requested_candidate, diagnostics)
}

fn compile_flat_source(
    identity: &SourceIdentity,
    flattened: &FlattenedSource,
    context: DesignContext,
) -> DesignCompileResult {
    let source = &flattened.source;
    let origins = &flattened.origins;
    let colours = match compile_colour_tokens(source, context.clone()) {
        Ok(success) => success,
        Err(failure) => {
            return DesignCompileResult::Fatal(DesignCompileFailure {
                attempted_source: identity.clone(),
                diagnostics: failure.diagnostics,
            });
        }
    };

    let mapping =
        match crate::mapping::compile_button_mapping_artifacts(source, &colours.value, context) {
            Ok(success) => success,
            Err(failure) => {
                let mut diagnostics = colours.diagnostics;
                diagnostics.extend(failure.diagnostics);
                return DesignCompileResult::Fatal(DesignCompileFailure {
                    attempted_source: identity.clone(),
                    diagnostics,
                });
            }
        };

    let mut provenance = dictionary_provenance(&colours.value, &mapping.value, origins);
    provenance.extend(mapping.value.provenance.clone());
    let mut diagnostics = colours.diagnostics;
    diagnostics.extend(mapping.diagnostics);
    DesignCompileResult::Success(DesignCompileSuccess {
        candidate: UnstampedResolvedDesign::new(
            identity.clone(),
            ResolvedTables {
                button: mapping.value.table,
            },
            ResolvedDictionary {
                colours: colours.value,
                metrics: mapping
                    .value
                    .metrics
                    .iter()
                    .map(|(name, metric)| (name.clone(), metric.resolved()))
                    .collect(),
                scales: mapping
                    .value
                    .scales
                    .iter()
                    .map(|(name, scale)| (name.clone(), scale.values.clone()))
                    .collect(),
            },
            mapping.value.typography,
            provenance,
        ),
        diagnostics,
    })
}

fn validate_scale_names(source: &DesignV1Source) -> Vec<DesignDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut check = |primitives: &crate::source::PrimitiveSource, prefix: &str| {
        for name in primitives.scales.keys() {
            if !crate::mapping::scale_name_is_flattenable(name) {
                diagnostics.push(DesignDiagnostic::error(
                    "invalid-scale-name",
                    format!("{prefix}.primitives.scales.{name}"),
                    crate::mapping::SCALE_NAME_RULE,
                ));
            }
        }
    };
    check(&source.primitives, "design.v1");
    for (index, block) in source.modifiers.iter().enumerate() {
        check(&block.primitives, &format!("design.v1.modifiers[{index}]"));
    }
    diagnostics
}

/// Intrinsic properties of an authored value are validated wherever the value
/// is authored; properties that depend on what else won in a context are
/// validated per-context after flattening. In particular, a step's integral
/// index and named-scale existence are intrinsic, while index-in-range depends
/// on the winning whole-vector scale replacement.
fn validate_authored_metrics(source: &DesignV1Source) -> Vec<DesignDiagnostic> {
    let mut diagnostics = Vec::new();
    // Modifier path validation forbids introducing a scale absent from the
    // base. The reachable name set is therefore these base names plus the
    // compiler-derived `radius` scale in every context.
    let available_scales = crate::mapping::authored_scale_names(source);
    let mut validate = |primitives: &crate::source::PrimitiveSource, prefix: &str| {
        for (name, metric) in &primitives.metrics {
            diagnostics.extend(crate::mapping::validate_authored_metric_shape(
                metric,
                &format!("{prefix}.primitives.metrics.{name}"),
                crate::mapping::AuthoredMetricPosition::Metric,
                &available_scales,
            ));
        }
        for (name, scale) in &primitives.scales {
            if name == crate::mapping::RADIUS_SCALE_NAME {
                diagnostics.push(crate::mapping::derived_scale_authored_diagnostic(format!(
                    "{prefix}.primitives.scales.radius"
                )));
                // Contents of a reserved scale are irrelevant: the vector may
                // not be authored at all, so do not diagnose its entry shapes.
                continue;
            }
            for (index, metric) in scale.iter().enumerate() {
                diagnostics.extend(crate::mapping::validate_authored_metric_shape(
                    metric,
                    &format!("{prefix}.primitives.scales.{name}[{index}]"),
                    crate::mapping::AuthoredMetricPosition::ScaleEntry,
                    &available_scales,
                ));
            }
        }
    };
    validate(&source.primitives, "design.v1");
    for (index, block) in source.modifiers.iter().enumerate() {
        validate(&block.primitives, &format!("design.v1.modifiers[{index}]"));
    }
    diagnostics
}

fn validate_modifiers(source: &DesignV1Source) -> Vec<DesignDiagnostic> {
    let mut diagnostics = Vec::new();
    let claimed = source
        .resolution_order
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let authored = source
        .modifiers
        .iter()
        .flat_map(|block| block.when.keys().copied())
        .collect::<BTreeSet<_>>();

    let mut seen_axes = BTreeSet::new();
    for (index, axis) in source.resolution_order.iter().copied().enumerate() {
        if !seen_axes.insert(axis) {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-axis-duplicate-in-resolution-order",
                format!("design.v1.resolution_order[{index}]"),
                format!(
                    "axis `{}` occurs more than once in resolution_order",
                    axis_name(axis)
                ),
            ));
        }
        if !authored.contains(&axis) {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-axis-claimed-without-block",
                format!("design.v1.resolution_order[{index}]"),
                format!(
                    "axis `{}` is claimed but no modifier `when` names it",
                    axis_name(axis)
                ),
            ));
        }
    }

    let mut selectors = BTreeSet::new();
    for (index, block) in source.modifiers.iter().enumerate() {
        let path = format!("design.v1.modifiers[{index}]");
        if block.when.is_empty() {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-when-empty",
                format!("{path}.when"),
                "modifier `when` cannot be empty; unconditional values belong in the base source",
            ));
        }
        for (&axis, value) in &block.when {
            let selector_path = format!("{path}.when.{}", axis_name(axis));
            if !claimed.contains(&axis) {
                diagnostics.push(DesignDiagnostic::error(
                    "modifier-axis-not-in-resolution-order",
                    &selector_path,
                    format!(
                        "selector {}={} names axis `{}` absent from resolution_order",
                        axis_name(axis),
                        value,
                        axis_name(axis)
                    ),
                ));
            }
            validate_modifier_value(axis, value, &selector_path, &mut diagnostics);
        }
        if !selectors.insert(block.when.clone()) {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-block-duplicate",
                format!("{path}.when"),
                format!(
                    "a modifier block already exists for `when: {}`",
                    format_selector(&block.when)
                ),
            ));
        }
        if block.families {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-alters-family-mapping",
                format!("{path}.families"),
                format!(
                    "modifier `when: {}` cannot alter family mappings",
                    format_selector(&block.when)
                ),
            ));
        }
        if block.typography {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-alters-family-mapping",
                format!("{path}.typography"),
                format!(
                    "modifier `when: {}` cannot alter typography or family structure",
                    format_selector(&block.when)
                ),
            ));
        }
        validate_modifier_paths(source, block, &path, &mut diagnostics);
        validate_modifier_metric_kinds(source, block, &path, &mut diagnostics);
    }
    validate_modifier_conflicts(source, &mut diagnostics);
    diagnostics
}

fn validate_modifier_metric_kinds(
    source: &DesignV1Source,
    block: &ModifierBlockSource,
    block_path: &str,
    diagnostics: &mut Vec<DesignDiagnostic>,
) {
    for (name, replacement) in &block.primitives.metrics {
        let Some(base) = source.primitives.metrics.get(name) else {
            // The generic modifier-path validator owns the missing-base error.
            continue;
        };
        let (Some(base_kind), Some(replacement_kind)) = (
            authored_metric_kind(base),
            authored_metric_kind(replacement),
        ) else {
            // The pre-flatten authored-shape validator owns malformed,
            // untagged, and unknown kinds.
            continue;
        };
        if base_kind == replacement_kind {
            continue;
        }

        let base_path = format!("design.v1.primitives.metrics.{name}");
        diagnostics.push(DesignDiagnostic::error(
            "modifier-metric-kind-change",
            format!("{block_path}.primitives.metrics.{name}"),
            format!(
                "modifier replacement is `{replacement_kind}` but base metric `{base_path}` is `{base_kind}`; a modifier must preserve the authored metric kind"
            ),
        ));
    }
}

fn authored_metric_kind(metric: &crate::source::MetricSource) -> Option<&str> {
    let crate::source::MetricSource::Tagged(metric) = metric else {
        return None;
    };
    matches!(metric.kind.as_str(), "px" | "step" | "ratio").then_some(metric.kind.as_str())
}

/// Whether one `when` entry names a value a compiled context can carry.
///
/// Both the diagnostic and the conflict-eligibility question are answered from
/// this one classification. Asking them from two separate matches would let
/// them diverge exactly once — when the `app` axis is implemented, or a new
/// fixed value is added, in one place and not the other — and the divergence
/// is silent: the block compiles, is skipped by conflict analysis, and an
/// equal-key overlap falls through to the source-index tiebreak, which is the
/// document-order dependence §1.6 exists to forbid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectorAdmission {
    Selectable,
    UnknownValue,
}

fn classify_selector_value(axis: ModifierAxis, value: &str) -> SelectorAdmission {
    let known = match axis {
        ModifierAxis::Scheme => Scheme::from_name(value).is_some(),
        ModifierAxis::Mode => Mode::from_name(value).is_some(),
        ModifierAxis::Contrast => Contrast::from_name(value).is_some(),
        ModifierAxis::App => {
            return if value.is_empty() {
                SelectorAdmission::UnknownValue
            } else {
                SelectorAdmission::Selectable
            };
        }
    };
    if known {
        SelectorAdmission::Selectable
    } else {
        SelectorAdmission::UnknownValue
    }
}

fn validate_modifier_value(
    axis: ModifierAxis,
    value: &str,
    path: &str,
    diagnostics: &mut Vec<DesignDiagnostic>,
) {
    match classify_selector_value(axis, value) {
        SelectorAdmission::Selectable => {}
        SelectorAdmission::UnknownValue => diagnostics.push(DesignDiagnostic::error(
            "modifier-axis-value-unknown",
            path,
            format!(
                "`{}` is not a fixed value of the `{}` axis",
                value,
                axis_name(axis)
            ),
        )),
    }
}

fn validate_modifier_conflicts(source: &DesignV1Source, diagnostics: &mut Vec<DesignDiagnostic>) {
    // §1.6 defines the conflict over two *selected* blocks, so a block no
    // context can select is not one of them. Reporting it anyway would answer
    // an unasked question — the author's actual fault is the unselectable
    // selector, already reported — and would vanish on fixing that fault,
    // which is the signature of a diagnostic pointing at the wrong thing.
    for (left_index, left) in source.modifiers.iter().enumerate() {
        let Some(left_key) = selectable_modifier_specificity(left, &source.resolution_order) else {
            continue;
        };
        let left_paths = modifier_token_paths(left);
        for (right_index, right) in source.modifiers.iter().enumerate().skip(left_index + 1) {
            if selectable_modifier_specificity(right, &source.resolution_order) != Some(left_key)
                || !modifiers_are_compatible(left, right)
            {
                continue;
            }
            let left_block_path = format!("design.v1.modifiers[{left_index}]");
            let right_block_path = format!("design.v1.modifiers[{right_index}]");
            let right_paths = modifier_token_paths(right);
            if let Some(shared_path) = left_paths.intersection(&right_paths).next() {
                let suffix = shared_path
                    .strip_prefix("design.v1")
                    .expect("modifier token paths use the design.v1 prefix");
                diagnostics.push(DesignDiagnostic::error(
                    "modifier-conflict",
                    format!("{right_block_path}{suffix}"),
                    format!(
                        "blocks `{left_block_path}` (`when: {}`) and `{right_block_path}` (`when: {}`) have specificity key ({}, {}) and both write `{shared_path}`",
                        format_selector(&left.when),
                        format_selector(&right.when),
                        left_key.0,
                        left_key.1,
                    ),
                ));
            }
        }
    }
}

fn modifier_specificity(
    block: &ModifierBlockSource,
    resolution_order: &[ModifierAxis],
) -> Option<(usize, usize)> {
    if block.when.is_empty() {
        return None;
    }
    let last_axis_position = block
        .when
        .keys()
        .map(|axis| {
            resolution_order
                .iter()
                .position(|candidate| candidate == axis)
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
        .expect("a non-empty selector has a last axis");
    Some((block.when.len(), last_axis_position))
}

/// The specificity key of a block some reachable context can actually select,
/// or `None` for one no context can — an empty `when`, an axis outside
/// `resolution_order`, an unknown fixed value, or the unimplemented `app`
/// axis. Every `None` case here is separately fatal in the same validation
/// pass, so skipping such a block never lets a real ambiguity through.
fn selectable_modifier_specificity(
    block: &ModifierBlockSource,
    resolution_order: &[ModifierAxis],
) -> Option<(usize, usize)> {
    let selectable = block.when.iter().all(|(axis, value)| {
        classify_selector_value(*axis, value) == SelectorAdmission::Selectable
    });
    if !selectable {
        return None;
    }
    modifier_specificity(block, resolution_order)
}

fn modifiers_are_compatible(left: &ModifierBlockSource, right: &ModifierBlockSource) -> bool {
    left.when
        .iter()
        .all(|(axis, value)| right.when.get(axis).is_none_or(|other| other == value))
}

fn modifier_token_paths(block: &ModifierBlockSource) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    record_modifier_paths(
        &mut paths,
        &block.primitives.colors,
        "design.v1.primitives.colors",
    );
    record_modifier_paths(
        &mut paths,
        &block.primitives.metrics,
        "design.v1.primitives.metrics",
    );
    record_modifier_paths(
        &mut paths,
        &block.primitives.scales,
        "design.v1.primitives.scales",
    );
    record_modifier_paths(
        &mut paths,
        &block.semantics.pairs,
        "design.v1.semantics.pairs",
    );
    record_modifier_paths(
        &mut paths,
        &block.semantics.non_text,
        "design.v1.semantics.non_text",
    );
    paths
}

fn record_modifier_paths<T>(
    paths: &mut BTreeSet<String>,
    values: &BTreeMap<String, T>,
    prefix: &str,
) {
    paths.extend(values.keys().map(|name| format!("{prefix}.{name}")));
}

fn format_selector(when: &BTreeMap<ModifierAxis, String>) -> String {
    when.iter()
        .map(|(axis, value)| format!("{}: {value}", axis_name(*axis)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_modifier_paths(
    source: &DesignV1Source,
    block: &ModifierBlockSource,
    block_path: &str,
    diagnostics: &mut Vec<DesignDiagnostic>,
) {
    let selector = format_selector(&block.when);
    validate_overlay_map(
        &source.primitives.colors,
        &block.primitives.colors,
        &format!("{block_path}.primitives.colors"),
        &selector,
        None,
        diagnostics,
    );
    validate_overlay_map(
        &source.primitives.metrics,
        &block.primitives.metrics,
        &format!("{block_path}.primitives.metrics"),
        &selector,
        None,
        diagnostics,
    );
    validate_overlay_map(
        &source.primitives.scales,
        &block.primitives.scales,
        &format!("{block_path}.primitives.scales"),
        &selector,
        Some(crate::mapping::RADIUS_SCALE_NAME),
        diagnostics,
    );
    validate_overlay_map(
        &source.semantics.pairs,
        &block.semantics.pairs,
        &format!("{block_path}.semantics.pairs"),
        &selector,
        None,
        diagnostics,
    );
    validate_overlay_map(
        &source.semantics.non_text,
        &block.semantics.non_text,
        &format!("{block_path}.semantics.non_text"),
        &selector,
        None,
        diagnostics,
    );
}

fn validate_overlay_map<T: PartialEq>(
    base: &std::collections::BTreeMap<String, T>,
    overlay: &std::collections::BTreeMap<String, T>,
    path: &str,
    selector: &str,
    ignored_name: Option<&str>,
    diagnostics: &mut Vec<DesignDiagnostic>,
) {
    for (name, value) in overlay {
        if ignored_name == Some(name) {
            continue;
        }
        let value_path = format!("{path}.{name}");
        let Some(base_value) = base.get(name) else {
            diagnostics.push(DesignDiagnostic::error(
                "modifier-introduces-unknown-path",
                value_path,
                format!(
                    "modifier `when: {selector}` writes path `{name}`, which does not exist in the base source; declare it in the base source first"
                ),
            ));
            continue;
        };
        if value == base_value {
            diagnostics.push(DesignDiagnostic::warning(
                "modifier-restates-base",
                value_path,
                format!("modifier `when: {selector}` value is identical to the base value"),
            ));
        }
    }
}

fn claimed_axes(resolution_order: &[ModifierAxis]) -> Vec<ModifierAxis> {
    let mut seen = BTreeSet::new();
    resolution_order
        .iter()
        .copied()
        .filter(|axis| seen.insert(*axis))
        .collect()
}

fn reachable_contexts(
    requested: DesignContext,
    axes: &[ModifierAxis],
    source: &DesignV1Source,
) -> Vec<DesignContext> {
    let requested_app = requested.app.clone();
    let mut contexts = vec![requested];
    for axis in axes {
        if *axis == ModifierAxis::App {
            let app_values = std::iter::once(None)
                .chain(std::iter::once(requested_app.clone()))
                .chain(
                    source
                        .modifiers
                        .iter()
                        .filter_map(|block| block.when.get(&ModifierAxis::App).cloned().map(Some)),
                )
                .collect::<BTreeSet<_>>();
            contexts = contexts
                .into_iter()
                .flat_map(|context| {
                    app_values.iter().cloned().map(move |app| DesignContext {
                        app,
                        ..context.clone()
                    })
                })
                .collect();
        } else {
            contexts = contexts
                .into_iter()
                .flat_map(|context| axis_contexts(context, *axis))
                .collect();
        }
    }
    contexts
}

fn axis_contexts(context: DesignContext, axis: ModifierAxis) -> Vec<DesignContext> {
    match axis {
        ModifierAxis::Scheme => Scheme::ALL
            .into_iter()
            .map(|scheme| DesignContext {
                scheme,
                ..context.clone()
            })
            .collect(),
        ModifierAxis::Mode => Mode::ALL
            .into_iter()
            .map(|mode| DesignContext {
                mode,
                ..context.clone()
            })
            .collect(),
        ModifierAxis::Contrast => Contrast::ALL
            .into_iter()
            .map(|contrast| DesignContext {
                contrast,
                ..context.clone()
            })
            .collect(),
        ModifierAxis::App => vec![context],
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValueOrigin {
    Base,
    Modifier(usize),
}

#[derive(Debug)]
struct SourceOrigins {
    values: BTreeMap<String, ValueOrigin>,
}

impl SourceOrigins {
    fn from_base(source: &DesignV1Source) -> Self {
        let mut values = BTreeMap::new();
        record_base_origins(
            &mut values,
            &source.primitives.colors,
            "design.v1.primitives.colors",
        );
        record_base_origins(
            &mut values,
            &source.primitives.metrics,
            "design.v1.primitives.metrics",
        );
        record_base_scale_origins(
            &mut values,
            &source.primitives.scales,
            "design.v1.primitives.scales",
        );
        record_base_origins(
            &mut values,
            &source.semantics.pairs,
            "design.v1.semantics.pairs",
        );
        record_base_origins(
            &mut values,
            &source.semantics.non_text,
            "design.v1.semantics.non_text",
        );
        Self { values }
    }

    fn rule_path(&self, base_path: &str) -> String {
        match self.values.get(base_path) {
            Some(ValueOrigin::Modifier(index)) => {
                if let Some(suffix) = base_path.strip_prefix("design.v1") {
                    format!("design.v1.modifiers[{index}]{suffix}")
                } else {
                    base_path.to_owned()
                }
            }
            Some(ValueOrigin::Base) | None => base_path.to_owned(),
        }
    }

    fn repoint_diagnostic(&self, diagnostic: &mut DesignDiagnostic) {
        diagnostic.path = self.rule_path(&diagnostic.path);
    }
}

#[derive(Debug)]
struct FlattenedSource {
    source: DesignV1Source,
    origins: SourceOrigins,
}

fn record_base_origins<T>(
    origins: &mut BTreeMap<String, ValueOrigin>,
    values: &BTreeMap<String, T>,
    prefix: &str,
) {
    origins.extend(
        values
            .keys()
            .map(|name| (format!("{prefix}.{name}"), ValueOrigin::Base)),
    );
}

fn record_base_scale_origins<T>(
    origins: &mut BTreeMap<String, ValueOrigin>,
    values: &BTreeMap<String, Vec<T>>,
    prefix: &str,
) {
    for (name, entries) in values {
        origins.insert(format!("{prefix}.{name}"), ValueOrigin::Base);
        origins.extend(
            entries
                .iter()
                .enumerate()
                .map(|(index, _)| (format!("{prefix}.{name}[{index}]"), ValueOrigin::Base)),
        );
    }
}

fn apply_overlay<T: Clone>(
    target: &mut BTreeMap<String, T>,
    overlay: &BTreeMap<String, T>,
    prefix: &str,
    block_index: usize,
    origins: &mut SourceOrigins,
) {
    for (name, value) in overlay {
        target.insert(name.clone(), value.clone());
        origins.values.insert(
            format!("{prefix}.{name}"),
            ValueOrigin::Modifier(block_index),
        );
    }
}

fn apply_scale_overlay<T: Clone>(
    target: &mut BTreeMap<String, Vec<T>>,
    overlay: &BTreeMap<String, Vec<T>>,
    prefix: &str,
    block_index: usize,
    origins: &mut SourceOrigins,
) {
    for (name, entries) in overlay {
        target.insert(name.clone(), entries.clone());
        let aggregate_path = format!("{prefix}.{name}");
        origins
            .values
            .insert(aggregate_path.clone(), ValueOrigin::Modifier(block_index));
        origins
            .values
            .retain(|path, _| !path.starts_with(&format!("{aggregate_path}[")));
        origins
            .values
            .extend(entries.iter().enumerate().map(|(index, _)| {
                (
                    format!("{aggregate_path}[{index}]"),
                    ValueOrigin::Modifier(block_index),
                )
            }));
    }
}

fn flatten_source(source: &DesignV1Source, context: DesignContext) -> FlattenedSource {
    let mut flattened = source.clone();
    flattened.modifiers.clear();
    let mut origins = SourceOrigins::from_base(source);
    let mut selected_blocks = source
        .modifiers
        .iter()
        .enumerate()
        .filter(|(_, block)| modifier_selects_context(block, &context))
        .collect::<Vec<_>>();
    selected_blocks.sort_by_key(|(index, block)| {
        (
            modifier_specificity(block, &source.resolution_order)
                .expect("validated modifier selectors have a specificity key"),
            *index,
        )
    });
    for (block_index, block) in selected_blocks {
        apply_overlay(
            &mut flattened.primitives.colors,
            &block.primitives.colors,
            "design.v1.primitives.colors",
            block_index,
            &mut origins,
        );
        apply_overlay(
            &mut flattened.primitives.metrics,
            &block.primitives.metrics,
            "design.v1.primitives.metrics",
            block_index,
            &mut origins,
        );
        apply_scale_overlay(
            &mut flattened.primitives.scales,
            &block.primitives.scales,
            "design.v1.primitives.scales",
            block_index,
            &mut origins,
        );
        apply_overlay(
            &mut flattened.semantics.pairs,
            &block.semantics.pairs,
            "design.v1.semantics.pairs",
            block_index,
            &mut origins,
        );
        apply_overlay(
            &mut flattened.semantics.non_text,
            &block.semantics.non_text,
            "design.v1.semantics.non_text",
            block_index,
            &mut origins,
        );
    }
    FlattenedSource {
        source: flattened,
        origins,
    }
}

fn modifier_selects_context(block: &ModifierBlockSource, context: &DesignContext) -> bool {
    block
        .when
        .iter()
        .all(|(axis, value)| context_value(context, *axis) == value)
}

fn same_claimed_context(
    left: &DesignContext,
    right: &DesignContext,
    axes: &[ModifierAxis],
) -> bool {
    axes.iter()
        .all(|axis| context_value(left, *axis) == context_value(right, *axis))
}

fn qualify_context(
    mut diagnostic: DesignDiagnostic,
    axes: &[ModifierAxis],
    context: DesignContext,
) -> DesignDiagnostic {
    // A repointed modifier path answers "where was this authored"; the context
    // answers "where does it fail". They are different questions and a
    // context-specific diagnostic needs both — exempting modifier paths here
    // would report a one-scheme contrast failure with no scheme named.
    if axes.is_empty() {
        return diagnostic;
    }
    let coordinates = axes
        .iter()
        .map(|axis| format!("{}={}", axis_name(*axis), context_value(&context, *axis)))
        .collect::<Vec<_>>()
        .join(",");
    if let Some(suffix) = diagnostic.path.strip_prefix("design.v1") {
        diagnostic.path = format!("design.v1[{coordinates}]{suffix}");
    } else {
        diagnostic.path = format!("design.v1[{coordinates}].{}", diagnostic.path);
    }
    diagnostic
}

fn axis_name(axis: ModifierAxis) -> &'static str {
    match axis {
        ModifierAxis::Scheme => "scheme",
        ModifierAxis::Mode => "mode",
        ModifierAxis::Contrast => "contrast",
        ModifierAxis::App => "app",
    }
}

fn context_value(context: &DesignContext, axis: ModifierAxis) -> &str {
    match axis {
        ModifierAxis::Scheme => context.scheme.name(),
        ModifierAxis::Mode => context.mode.name(),
        ModifierAxis::Contrast => context.contrast.name(),
        ModifierAxis::App => context.app.as_deref().unwrap_or(""),
    }
}

fn has_errors(diagnostics: &[DesignDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
}

fn deduplicate_diagnostics(diagnostics: &mut Vec<DesignDiagnostic>) {
    let mut seen = BTreeSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.code,
            diagnostic.path.clone(),
            diagnostic.message.clone(),
        ))
    });
}

type DiagnosticKey = (&'static str, String, String);

fn diagnostic_key(diagnostic: &DesignDiagnostic) -> DiagnosticKey {
    (
        diagnostic.code,
        diagnostic.path.clone(),
        diagnostic.message.clone(),
    )
}

fn contextualize_diagnostics(
    diagnostics: Vec<(DesignDiagnostic, DesignContext)>,
    axes: &[ModifierAxis],
    reachable_count: usize,
) -> Vec<DesignDiagnostic> {
    let mut counts = BTreeMap::new();
    for (diagnostic, _) in &diagnostics {
        *counts.entry(diagnostic_key(diagnostic)).or_insert(0usize) += 1;
    }

    let mut emitted_invariant = BTreeSet::new();
    diagnostics
        .into_iter()
        .filter_map(|(diagnostic, context)| {
            let key = diagnostic_key(&diagnostic);
            if counts.get(&key).copied() == Some(reachable_count) {
                emitted_invariant.insert(key).then_some(diagnostic)
            } else {
                Some(qualify_context(diagnostic, axes, context))
            }
        })
        .collect()
}

fn finish_compile(
    document: &DesignSourceDocument,
    requested_candidate: Option<UnstampedResolvedDesign>,
    mut diagnostics: Vec<DesignDiagnostic>,
) -> DesignCompileResult {
    deduplicate_diagnostics(&mut diagnostics);
    if has_errors(&diagnostics) {
        return fatal(document, diagnostics);
    }
    let Some(candidate) = requested_candidate else {
        diagnostics.push(DesignDiagnostic::error(
            "internal-requested-context-missing",
            "design.v1",
            "the requested context did not produce an artifact",
        ));
        return fatal(document, diagnostics);
    };

    DesignCompileResult::Success(DesignCompileSuccess {
        candidate,
        diagnostics,
    })
}

fn fatal(
    document: &DesignSourceDocument,
    diagnostics: Vec<DesignDiagnostic>,
) -> DesignCompileResult {
    DesignCompileResult::Fatal(DesignCompileFailure {
        attempted_source: document.identity.clone(),
        diagnostics,
    })
}

fn dictionary_provenance(
    colours: &crate::ResolvedColours,
    mapping: &crate::mapping::CompiledButtonMapping,
    origins: &SourceOrigins,
) -> DesignProvenance {
    let mut provenance = DesignProvenance::default();
    for name in colours.primitives.keys() {
        let rule = origins.rule_path(&format!("design.v1.primitives.colors.{name}"));
        provenance.insert(
            DesignValueId::ColourPrimitive(name.clone()),
            authored(rule, Vec::new()),
        );
    }
    for (name, pair) in &colours.pairs {
        let rule = origins.rule_path(&format!("design.v1.semantics.pairs.{name}"));
        let token_path = pair.recipe.as_ref().map_or_else(
            || {
                let mut path = vec![
                    format!("dictionary.colours.primitives.{}", pair.surface_name),
                    format!("dictionary.colours.primitives.{}", pair.foreground_name),
                ];
                if let Some(backdrop) = &pair.backdrop_name {
                    path.push(format!("dictionary.colours.primitives.{backdrop}"));
                }
                path
            },
            recipe_token_path,
        );
        provenance.insert(
            DesignValueId::ColourPair(name.clone()),
            authored(rule, token_path),
        );
    }
    for (name, colour) in &colours.non_text {
        provenance.insert(
            DesignValueId::NonTextColour(name.clone()),
            authored(
                origins.rule_path(&format!("design.v1.semantics.non_text.{name}")),
                vec![format!(
                    "dictionary.colours.primitives.{}",
                    colour.value_name
                )],
            ),
        );
    }
    for (scale_name, scale) in &mapping.scales {
        for (index, value) in scale.values.iter().copied().enumerate() {
            let id = DesignValueId::ScaleEntry {
                scale: scale_name.clone(),
                index,
            };
            let entry_provenance = match &scale.origin {
                crate::mapping::CompiledScaleOrigin::Authored => {
                    let rule = origins.rule_path(&format!(
                        "design.v1.primitives.scales.{scale_name}[{index}]"
                    ));
                    authored_metric_value(
                        rule.clone(),
                        Vec::new(),
                        rule,
                        crate::AuthoredMetric::Px { value },
                    )
                }
                crate::mapping::CompiledScaleOrigin::Derived {
                    generator,
                    base_metric,
                } => {
                    let base_rule =
                        origins.rule_path(&format!("design.v1.primitives.metrics.{base_metric}"));
                    let authored_base = mapping
                        .metrics
                        .get(*base_metric)
                        .expect("a compiled derived scale retains its compiled base metric")
                        .authored();
                    authored_metric_value(
                        format!("compiler.generators.{generator}"),
                        vec![format!("dictionary.metrics.{base_metric}")],
                        base_rule,
                        authored_base,
                    )
                }
            };
            provenance.insert(id, entry_provenance);
        }
    }
    for (name, metric) in &mapping.metrics {
        let applied_rule = origins.rule_path(&format!("design.v1.primitives.metrics.{name}"));
        let authored_metric = metric.authored();
        let (token_path, value_origin_rule) = match &authored_metric {
            crate::AuthoredMetric::Step { scale, index } => {
                let scale_origin = &mapping.scales[scale].origin;
                let value_origin_rule = match scale_origin {
                    crate::mapping::CompiledScaleOrigin::Authored => {
                        origins.rule_path(&format!("design.v1.primitives.scales.{scale}[{index}]"))
                    }
                    crate::mapping::CompiledScaleOrigin::Derived { generator, .. } => {
                        format!("compiler.generators.{generator}")
                    }
                };
                (
                    vec![format!("dictionary.scales.{scale}[{index}]")],
                    value_origin_rule,
                )
            }
            crate::AuthoredMetric::Px { .. } | crate::AuthoredMetric::Ratio { .. } => {
                (Vec::new(), applied_rule.clone())
            }
        };
        provenance.insert(
            DesignValueId::Metric(name.clone()),
            authored_metric_value(applied_rule, token_path, value_origin_rule, authored_metric),
        );
    }
    for (name, record) in mapping.typography.scale() {
        provenance.insert(
            DesignValueId::TypeRecord(name.clone()),
            authored(
                format!("design.v1.typography.records.{name}"),
                vec![format!("dictionary.metrics.{}", record.font_size_metric)],
            ),
        );
    }
    provenance
}

fn recipe_token_path(recipe: &crate::DerivationRecipe) -> Vec<String> {
    recipe
        .bindings
        .iter()
        .flat_map(|binding| match binding {
            crate::RecipeBinding::Pair { name } => {
                vec![format!("dictionary.colours.pairs.{name}")]
            }
            crate::RecipeBinding::Colour { name, .. } => {
                let tier = if crate::NON_TEXT_NAMES.contains(&name.as_str()) {
                    "non_text"
                } else {
                    "primitives"
                };
                vec![format!("dictionary.colours.{tier}.{name}")]
            }
            crate::RecipeBinding::ColourList { names, .. } => names
                .iter()
                .map(|name| format!("dictionary.colours.primitives.{name}"))
                .collect(),
            crate::RecipeBinding::Ratio { name, .. } => {
                vec![format!("dictionary.metrics.{name}")]
            }
        })
        .collect()
}

fn authored(rule: String, token_path: Vec<String>) -> ValueProvenance {
    ValueProvenance {
        token_path,
        applied_rule: rule.clone(),
        value_origin_rule: rule,
        authored_metric: None,
        focus_ring: None,
    }
}

fn authored_metric_value(
    applied_rule: String,
    token_path: Vec<String>,
    value_origin_rule: String,
    authored_metric: crate::AuthoredMetric,
) -> ValueProvenance {
    ValueProvenance {
        token_path,
        applied_rule,
        value_origin_rule,
        authored_metric: Some(authored_metric),
        focus_ring: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ButtonCellKey, ButtonProperty, ButtonSize, ButtonVariant, ColourSpace, CoveragePolicy,
        DesignCompileResult, EMBEDDED_DEFAULT_SOURCE, InteractionState, MetricSource,
        ModifierBlockSource, OklchSource, PrimitiveSource, RecipeArgumentSource, SemanticSource,
        SourceIdentity, TaggedMetricSource, parse_design_source,
    };

    fn document() -> DesignSourceDocument {
        let mut document = parse_design_source(
            SourceIdentity::new("compiler-test"),
            EMBEDDED_DEFAULT_SOURCE,
        )
        .expect("embedded fixture parses");
        // Modifier-law tests construct the complete axis scenario they mean
        // to exercise. Keep their baseline pinned to the embedded source's
        // Ocean/Light base rather than inheriting its shipped context matrix.
        document.v1.resolution_order.clear();
        document.v1.modifiers.clear();
        // Modifier-law fixtures need an admitted override product in every
        // synthetic context. The embedded selection pair is intentionally at
        // an endpoint in the modifier-free Dark context, so use the safe
        // authored secondary pair under the same closed semantic names. Keep
        // those names intact: focus-ring recipes bind the cell's painted pair
        // by name as well as by delivered value.
        let secondary = document.v1.semantics.pairs["secondary"].clone();
        document
            .v1
            .semantics
            .pairs
            .insert("primary".into(), secondary.clone());
        document
            .v1
            .semantics
            .pairs
            .insert("accent".into(), secondary);
        // The helper deliberately changes values named by four shipped
        // crosswalk rows. Keep its synthetic v0 side equivalent so tests of
        // unrelated compiler laws still reach the law they are exercising.
        document.legacy.control = Some("#e9f4f7".into());
        document.legacy.row_selected = Some("#e9f4f7".into());
        document.legacy.row_selected_text = Some("#002631".into());
        document.legacy.row_selected_text_dim = Some("#002631".into());
        for field in [
            "control",
            "row_selected",
            "row_selected_text",
            "row_selected_text_dim",
        ] {
            let crate::V0CrosswalkExpressionSource::Pair { value, .. } = document
                .v1
                .v0_crosswalk
                .get_mut(field)
                .expect("shipped crosswalk row")
            else {
                panic!("{field} must remain a pair crosswalk row")
            };
            *value = "secondary".into();
        }
        document
    }

    fn document_with_admitted_product_faults() -> DesignSourceDocument {
        let mut document = document();
        let button = document.v1.families.button.as_mut().unwrap();
        for rule in &mut button.compound_variants {
            if matches!(
                rule.when.variant,
                Some(ButtonVariant::Primary | ButtonVariant::Ghost)
            ) && matches!(
                rule.when.interaction,
                Some(
                    InteractionState::Hovered
                        | InteractionState::Pressed
                        | InteractionState::Disabled
                )
            ) {
                if let Some(crate::MappingValueSource::Derive { args, .. }) =
                    rule.set.get_mut(&ButtonProperty::Pair)
                {
                    let RecipeArgumentSource::Pair { value } = &mut args[0] else {
                        unreachable!()
                    };
                    *value = "secondary".into();
                }
                if let Some(crate::MappingValueSource::Derive { args, .. }) =
                    rule.set.get_mut(&ButtonProperty::Ring)
                {
                    let RecipeArgumentSource::Pair { value } = &mut args[1] else {
                        unreachable!()
                    };
                    *value = "secondary".into();
                }
            }
        }
        for interaction in [InteractionState::Hovered, InteractionState::Pressed] {
            button.compound_variants.push(crate::MappingRuleSource {
                when: crate::MappingSelectorSource {
                    variant: Some(ButtonVariant::Primary),
                    interaction: Some(interaction),
                    focus_visible: Some(true),
                    ..Default::default()
                },
                set: BTreeMap::from([(
                    ButtonProperty::Ring,
                    crate::MappingValueSource::Derive {
                        name: "focus_ring".into(),
                        args: vec![
                            RecipeArgumentSource::Colour {
                                value: "ring".into(),
                            },
                            RecipeArgumentSource::Pair {
                                value: "secondary".into(),
                            },
                        ],
                    },
                )]),
            });
        }
        let embedded = parse_design_source(
            SourceIdentity::new("compiler-fault-source"),
            EMBEDDED_DEFAULT_SOURCE,
        )
        .expect("embedded fault source parses");
        let primary = embedded.v1.semantics.pairs["primary"].clone();
        let mut accent = primary.clone();
        let crate::PairSource::Derived { derive } = &mut accent else {
            panic!("the embedded primary fault source must remain derived")
        };
        let [
            RecipeArgumentSource::Colour { .. },
            RecipeArgumentSource::Colour {
                value: panel_anchor,
            },
        ] = derive.args.as_mut_slice()
        else {
            panic!("the primary fault source must retain selection_pair's signature")
        };
        // Both are deliberately bad endpoint selection pairs. Accent uses a
        // test-only, slightly lower-chroma panel anchor so it delivers a
        // different foreground byte while retaining all eight product faults.
        document.v1.primitives.colors.insert(
            "status.success".into(),
            OklchSource {
                color_space: ColourSpace::Oklch,
                l: 0.96,
                c: 0.01,
                h: 220.0,
                alpha: 1.0,
            },
        );
        *panel_anchor = "status.success".into();
        document
            .v1
            .semantics
            .pairs
            .insert("primary".into(), primary);
        document.v1.semantics.pairs.insert("accent".into(), accent);
        document
    }

    fn block(axis: ModifierAxis, value: &str) -> ModifierBlockSource {
        ModifierBlockSource {
            when: [(axis, value.into())].into_iter().collect(),
            primitives: PrimitiveSource::default(),
            semantics: SemanticSource::default(),
            families: false,
            typography: false,
        }
    }

    #[test]
    fn synthetic_dark_rejects_every_admitted_override_product_fault() {
        let document = document_with_admitted_product_faults();
        let context = DesignContext {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
            contrast: Contrast::Normal,
            app: None,
        };
        let flattened = flatten_source(&document.v1, context.clone());
        let colours = compile_colour_tokens(&flattened.source, context.clone())
            .expect("both deliberately faulty pairs must resolve before product validation")
            .value;
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
            "the two faulty inputs must have distinct delivered bytes"
        );
        let DesignCompileResult::Fatal(failure) =
            compile_flat_source(&document.identity, &flattened, context)
        else {
            panic!("synthetic Dark point must fail its admitted override product")
        };
        let faults = failure
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "override-product")
            .collect::<Vec<_>>();
        assert_eq!(faults.len(), 16, "{:?}", failure.diagnostics);
        let fault_shapes = faults
            .iter()
            .map(|diagnostic| {
                let pair = if diagnostic.message.contains("pair `accent`") {
                    "accent"
                } else if diagnostic.message.contains("pair `primary`") {
                    "primary"
                } else {
                    panic!("fault does not identify its admitted pair: {diagnostic:?}")
                };
                (diagnostic.path.as_str(), pair)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            fault_shapes.len(),
            16,
            "each faulty pair must produce eight distinct path/pair cases"
        );
        for pair in ["primary", "accent"] {
            assert_eq!(
                fault_shapes
                    .iter()
                    .filter(|(_, fault_pair)| *fault_pair == pair)
                    .count(),
                8,
                "{pair} must contribute eight distinct product faults"
            );
        }
        assert!(faults.iter().all(|diagnostic| {
            diagnostic.severity == DiagnosticSeverity::Error
                && diagnostic.message.contains("admitted pair")
                && diagnostic
                    .message
                    .contains("permitted override failed its text-contrast postcondition")
                && !diagnostic.message.contains("unavailable in this context")
                && (diagnostic.message.contains("pair `accent`")
                    || diagnostic.message.contains("pair `primary`"))
                && (diagnostic.message.contains("contrast_safe_lift")
                    || diagnostic.message.contains("contrast_safe_toward")
                    || diagnostic.message.contains("disabled_pair"))
        }));
    }

    #[test]
    fn shipped_domain_exclusions_are_explicit_at_every_context() {
        let document = parse_design_source(
            SourceIdentity::new("embedded:override-domain-matrix"),
            EMBEDDED_DEFAULT_SOURCE,
        )
        .unwrap();
        let mut actual = BTreeMap::<(String, String, String), usize>::new();
        for scheme in Scheme::ALL {
            for mode in Mode::ALL {
                let context = DesignContext {
                    scheme,
                    mode,
                    contrast: Contrast::Normal,
                    app: None,
                };
                let flattened = flatten_source(&document.v1, context.clone());
                let result = compile_flat_source(&document.identity, &flattened, context);
                let DesignCompileResult::Success(success) = result else {
                    panic!("{} / {} failed: {result:#?}", scheme.name(), mode.name())
                };
                assert!(success.diagnostics.iter().all(|diagnostic| {
                    diagnostic.code != "override-product"
                        && diagnostic.code != "override-product-domain-exclusion"
                }));
                for variant in ButtonVariant::ALL {
                    for interaction in [
                        InteractionState::Hovered,
                        InteractionState::Pressed,
                        InteractionState::Disabled,
                    ] {
                        let cell = success.candidate.tables().button.cell(ButtonCellKey {
                            variant,
                            size: ButtonSize::Md,
                            interaction,
                            focus_visible: false,
                        });
                        let recipe = cell
                            .pair_recipe
                            .as_ref()
                            .expect("every shipped non-resting button cell retains a recipe");
                        let policy = recipe
                            .substitution_policy()
                            .expect("every shipped non-resting recipe is substitutable");
                        assert_eq!(
                            policy.decisions().count(),
                            success.candidate.dictionary().colours.pairs.len()
                        );
                        assert_eq!(
                            policy.decision("muted"),
                            Some(&crate::PairRefDecision::Excluded(
                                crate::PairRefExclusion::OutsideRecipeDomain {
                                    required: crate::RecipePairDomain::NonTransparentSurface,
                                }
                            ))
                        );
                        assert_eq!(
                            policy
                                .decisions()
                                .filter(|(_, decision)| matches!(
                                    decision,
                                    crate::PairRefDecision::Excluded(_)
                                ))
                                .map(|(name, _)| name)
                                .collect::<Vec<_>>(),
                            ["muted"]
                        );
                        *actual
                            .entry((
                                recipe.name.into(),
                                variant.name().into(),
                                interaction.name().into(),
                            ))
                            .or_default() += 1;
                    }
                }
            }
        }
        let expected = BTreeMap::from([
            (
                (
                    "contrast_safe_lift".into(),
                    "default".into(),
                    "hovered".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_lift".into(),
                    "primary".into(),
                    "hovered".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_lift".into(),
                    "destructive".into(),
                    "hovered".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_toward".into(),
                    "ghost".into(),
                    "hovered".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_toward".into(),
                    "default".into(),
                    "pressed".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_toward".into(),
                    "primary".into(),
                    "pressed".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_toward".into(),
                    "destructive".into(),
                    "pressed".into(),
                ),
                12,
            ),
            (
                (
                    "contrast_safe_toward".into(),
                    "ghost".into(),
                    "pressed".into(),
                ),
                12,
            ),
            (
                ("disabled_pair".into(), "default".into(), "disabled".into()),
                12,
            ),
            (
                ("disabled_pair".into(), "primary".into(), "disabled".into()),
                12,
            ),
            (
                (
                    "disabled_pair".into(),
                    "destructive".into(),
                    "disabled".into(),
                ),
                12,
            ),
            (
                ("disabled_pair".into(), "ghost".into(), "disabled".into()),
                12,
            ),
        ]);
        assert_eq!(actual, expected);
        assert_eq!(actual.values().sum::<usize>(), 144);
    }

    fn compound_block(when: &[(ModifierAxis, &str)]) -> ModifierBlockSource {
        ModifierBlockSource {
            when: when
                .iter()
                .map(|(axis, value)| (*axis, (*value).into()))
                .collect(),
            primitives: PrimitiveSource::default(),
            semantics: SemanticSource::default(),
            families: false,
            typography: false,
        }
    }

    fn colour(l: f64) -> OklchSource {
        OklchSource {
            color_space: ColourSpace::Oklch,
            l,
            c: 0.0,
            h: 0.0,
            alpha: 1.0,
        }
    }

    fn diagnostics(result: &DesignCompileResult) -> &[DesignDiagnostic] {
        match result {
            DesignCompileResult::Success(success) => &success.diagnostics,
            DesignCompileResult::Fatal(failure) => &failure.diagnostics,
        }
    }

    fn assert_fatal_code(document: &DesignSourceDocument, code: &str) {
        let result = compile_design(document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(
            diagnostics(&result)
                .iter()
                .any(|diagnostic| diagnostic.code == code),
            "missing diagnostic {code}: {:?}",
            diagnostics(&result)
        );
    }

    fn padding_step_from_decimal(identity: &'static str, decimal: &str) -> DesignSourceDocument {
        let replacement = format!(
            "\"button.padding_x\": {{ kind: \"step\", scale: \"spacing\", value: {decimal} }}"
        );
        let source = EMBEDDED_DEFAULT_SOURCE.replace(
            "\"button.padding_x\": { kind: \"step\", scale: \"spacing\", value: 5 }",
            &replacement,
        );
        parse_design_source(SourceIdentity::new(identity), &source)
            .expect("authored decimal remains parseable for a stable compiler diagnostic")
    }

    fn masked_padding_step_from_decimal(
        identity: &'static str,
        decimal: &str,
    ) -> DesignSourceDocument {
        let mut document = padding_step_from_decimal(identity, decimal);
        document.v1.resolution_order = vec![ModifierAxis::Mode];
        let mut light = block(ModifierAxis::Mode, "light");
        light.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 5.0),
        );
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 6.0),
        );
        document.v1.modifiers = vec![light, dark];
        document
    }

    fn parsed_padding_step_value(document: &DesignSourceDocument) -> f64 {
        let MetricSource::Tagged(metric) = &document.v1.primitives.metrics["button.padding_x"]
        else {
            panic!("fixture padding step is tagged")
        };
        metric.value
    }

    fn assert_masked_padding_step_is_fatal(document: &DesignSourceDocument) {
        let result = compile_design(document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let failures = diagnostics(&result)
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "invalid-metric"
                    && diagnostic.path == "design.v1.primitives.metrics.button.padding_x"
            })
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", diagnostics(&result));
        assert!(
            failures[0].message.contains("non-negative integer index"),
            "{:?}",
            failures[0]
        );
    }

    #[test]
    fn rejects_bare_metric_number() {
        let source = EMBEDDED_DEFAULT_SOURCE.replace(
            "\"button.height.md\": { kind: \"px\", value: 28.0 }",
            "\"button.height.md\": 28.0",
        );
        let document = parse_design_source(SourceIdentity::new("bare-metric"), &source)
            .expect("bare metric remains parseable for a stable compiler diagnostic");
        assert_fatal_code(&document, "metric-untagged");
    }

    #[test]
    fn rejects_bare_number_in_a_metric_scale() {
        let mut document = document();
        document
            .v1
            .primitives
            .scales
            .get_mut("type")
            .expect("type scale")[0] = MetricSource::Untagged(11.333);
        assert_fatal_code(&document, "metric-untagged");
    }

    #[test]
    fn rejects_unknown_metric_kind() {
        let mut document = document();
        document.v1.primitives.metrics.insert(
            "button.height.md".into(),
            MetricSource::Tagged(TaggedMetricSource {
                kind: "rem".into(),
                value: 28.0,
                scale: None,
            }),
        );
        assert_fatal_code(&document, "metric-kind-unknown");
    }

    #[test]
    fn rejects_step_naming_unknown_scale() {
        let mut document = document();
        document.v1.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("missing", 1.0),
        );
        document.v1.resolution_order = vec![ModifierAxis::Mode];
        let mut light = block(ModifierAxis::Mode, "light");
        light.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 5.0),
        );
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 6.0),
        );
        document.v1.modifiers = vec![light, dark];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let failures = diagnostics(&result)
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "metric-step-scale-unknown"
                    && diagnostic.path == "design.v1.primitives.metrics.button.padding_x"
            })
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", diagnostics(&result));
        assert!(failures[0].message.contains("`missing`"));
    }

    #[test]
    fn fixture_exposes_shipped_radius_and_authored_spacing_without_value_drift() {
        let result = compile_design(&document(), DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!("embedded metric bases failed: {:?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().scales["radius"],
            [2.0, 4.0, 6.0, 10.0]
        );
        assert_eq!(
            success.candidate.dictionary().scales["spacing"],
            [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 16.0, 20.0, 24.0]
        );
        assert_eq!(
            success.candidate.dictionary().metrics["radius.md"].value,
            4.0
        );
        assert_eq!(
            success.candidate.dictionary().metrics["button.padding_x"].value,
            10.0
        );
        assert_eq!(
            success
                .candidate
                .provenance()
                .value(&DesignValueId::Metric("radius.md".into()))
                .expect("radius.md provenance")
                .authored_metric,
            Some(crate::AuthoredMetric::Step {
                scale: "radius".into(),
                index: 1,
            })
        );
        assert_eq!(
            success
                .candidate
                .provenance()
                .value(&DesignValueId::Metric("button.padding_x".into()))
                .expect("button padding provenance")
                .authored_metric,
            Some(crate::AuthoredMetric::Step {
                scale: "spacing".into(),
                index: 5,
            })
        );
    }

    #[test]
    fn radius_scale_clamps_both_negative_offset_arms() {
        let mut document = document();
        document
            .v1
            .primitives
            .metrics
            .insert("radius".into(), MetricSource::px(1.0));

        let result = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!("low radius base failed: {:?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().scales["radius"],
            [0.0, 0.0, 1.0, 5.0]
        );
    }

    #[test]
    fn radius_base_is_required_and_must_be_px() {
        let path = "design.v1.primitives.metrics.radius";
        let mut missing = document();
        missing.v1.primitives.metrics.remove("radius");
        let missing_result = compile_design(&missing, DesignContext::default());
        let missing_diagnostic = diagnostics(&missing_result)
            .iter()
            .find(|diagnostic| diagnostic.code == "radius-base-missing")
            .expect("missing-radius diagnostic");
        assert!(matches!(missing_result, DesignCompileResult::Fatal(_)));
        assert_eq!(missing_diagnostic.path, path);

        for source in [MetricSource::ratio(6.0), MetricSource::step("type", 0.0)] {
            let mut wrong_kind = document();
            wrong_kind
                .v1
                .primitives
                .metrics
                .insert("radius".into(), source);
            let result = compile_design(&wrong_kind, DesignContext::default());
            let diagnostic = diagnostics(&result)
                .iter()
                .find(|diagnostic| diagnostic.code == "radius-base-not-px")
                .expect("wrong-kind radius diagnostic");
            assert!(matches!(result, DesignCompileResult::Fatal(_)));
            assert_eq!(diagnostic.path, path);
        }
    }

    #[test]
    fn malformed_authored_metrics_are_fatal_even_when_every_reachable_winner_is_valid() {
        let covered_base = |radius: MetricSource| {
            let mut document = document();
            document
                .v1
                .primitives
                .metrics
                .insert("radius".into(), radius);
            document.v1.resolution_order = vec![ModifierAxis::Mode];
            let mut light = block(ModifierAxis::Mode, "light");
            light
                .primitives
                .metrics
                .insert("radius".into(), MetricSource::px(6.0));
            let mut dark = block(ModifierAxis::Mode, "dark");
            dark.primitives
                .metrics
                .insert("radius".into(), MetricSource::px(8.0));
            document.v1.modifiers = vec![light, dark];
            document
        };
        let cases = [
            (
                covered_base(MetricSource::Untagged(6.0)),
                "metric-untagged",
                "design.v1.primitives.metrics.radius",
            ),
            (
                covered_base(MetricSource::Tagged(TaggedMetricSource {
                    kind: "length".into(),
                    value: 6.0,
                    scale: None,
                })),
                "metric-kind-unknown",
                "design.v1.primitives.metrics.radius",
            ),
            (
                covered_base(MetricSource::Tagged(TaggedMetricSource {
                    kind: "px".into(),
                    value: 6.0,
                    scale: Some("spacing".into()),
                })),
                "invalid-metric",
                "design.v1.primitives.metrics.radius",
            ),
            {
                let mut document = document();
                document.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
                let mut lower = block(ModifierAxis::Scheme, "crimson");
                lower
                    .primitives
                    .metrics
                    .insert("button.border_width".into(), MetricSource::Untagged(1.0));
                let mut light = compound_block(&[
                    (ModifierAxis::Scheme, "crimson"),
                    (ModifierAxis::Mode, "light"),
                ]);
                light
                    .primitives
                    .metrics
                    .insert("button.border_width".into(), MetricSource::px(2.0));
                let mut dark = compound_block(&[
                    (ModifierAxis::Scheme, "crimson"),
                    (ModifierAxis::Mode, "dark"),
                ]);
                dark.primitives
                    .metrics
                    .insert("button.border_width".into(), MetricSource::px(3.0));
                document.v1.modifiers = vec![lower, light, dark];
                (
                    document,
                    "metric-untagged",
                    "design.v1.modifiers[0].primitives.metrics.button.border_width",
                )
            },
            {
                let mut document = document();
                document.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
                let mut lower = block(ModifierAxis::Scheme, "crimson");
                lower
                    .primitives
                    .scales
                    .insert("type".into(), vec![MetricSource::Untagged(11.333)]);
                let valid_type = || vec![MetricSource::px(11.333), MetricSource::px(13.333)];
                let mut light = compound_block(&[
                    (ModifierAxis::Scheme, "crimson"),
                    (ModifierAxis::Mode, "light"),
                ]);
                light.primitives.scales.insert("type".into(), valid_type());
                let mut dark = compound_block(&[
                    (ModifierAxis::Scheme, "crimson"),
                    (ModifierAxis::Mode, "dark"),
                ]);
                dark.primitives.scales.insert("type".into(), valid_type());
                document.v1.modifiers = vec![lower, light, dark];
                (
                    document,
                    "metric-untagged",
                    "design.v1.modifiers[0].primitives.scales.type[0]",
                )
            },
        ];

        for (document, code, path) in cases {
            let result = compile_design(&document, DesignContext::default());
            assert!(matches!(result, DesignCompileResult::Fatal(_)));
            let authored_faults = diagnostics(&result)
                .iter()
                .filter(|diagnostic| diagnostic.code == code && diagnostic.path == path)
                .collect::<Vec<_>>();
            assert_eq!(
                authored_faults.len(),
                1,
                "{path}: {:?}",
                diagnostics(&result)
            );
        }
    }

    #[test]
    fn radius_scale_cannot_be_authored_in_base_or_modifier() {
        let mut base = document();
        base.v1
            .primitives
            .scales
            .insert("radius".into(), vec![MetricSource::Untagged(99.0)]);
        let base_result = compile_design(&base, DesignContext::default());
        let base_diagnostics = diagnostics(&base_result);
        let base_derived = base_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derived-scale-authored")
            .collect::<Vec<_>>();
        assert!(matches!(base_result, DesignCompileResult::Fatal(_)));
        assert_eq!(base_derived.len(), 1, "{base_diagnostics:?}");
        assert_eq!(base_derived[0].path, "design.v1.primitives.scales.radius");
        assert!(
            base_diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "metric-untagged"),
            "reserved scale contents were diagnosed: {base_diagnostics:?}"
        );

        let mut modified = document();
        modified.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut unreachable = block(ModifierAxis::Scheme, "not-a-scheme");
        unreachable
            .primitives
            .scales
            .insert("radius".into(), vec![MetricSource::Untagged(99.0)]);
        modified.v1.modifiers.push(unreachable);
        let modified_result = compile_design(&modified, DesignContext::default());
        let modified_diagnostics = diagnostics(&modified_result);
        let derived = modified_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "derived-scale-authored")
            .collect::<Vec<_>>();
        assert!(matches!(modified_result, DesignCompileResult::Fatal(_)));
        assert_eq!(derived.len(), 1, "{modified_diagnostics:?}");
        assert_eq!(
            derived[0].path,
            "design.v1.modifiers[0].primitives.scales.radius"
        );
        assert!(
            modified_diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "metric-untagged"),
            "reserved scale contents were diagnosed: {modified_diagnostics:?}"
        );
        assert!(
            modified_diagnostics.iter().any(|diagnostic| {
                diagnostic.code == "modifier-axis-value-unknown"
                    && diagnostic.path == "design.v1.modifiers[0].when.scheme"
            }),
            "test selector unexpectedly became reachable: {modified_diagnostics:?}"
        );
        assert!(
            modified_diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != "modifier-introduces-unknown-path"),
            "reserved scale fell through to the generic overlay diagnostic: {modified_diagnostics:?}"
        );
    }

    #[test]
    fn modifier_regenerates_radius_scale_and_provenance_from_winning_knob() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson
            .primitives
            .metrics
            .insert("radius".into(), MetricSource::px(8.0));
        document.v1.modifiers.push(crimson);

        let result = compile_design(
            &document,
            DesignContext {
                scheme: Scheme::Crimson,
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(success) = result else {
            panic!("modifier radius failed: {:?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().scales["radius"],
            [4.0, 6.0, 8.0, 12.0]
        );
        assert_eq!(
            success.candidate.dictionary().metrics["radius.md"].value,
            6.0
        );

        let winning_rule = "design.v1.modifiers[0].primitives.metrics.radius";
        let scale_entry = success
            .candidate
            .provenance()
            .value(&DesignValueId::ScaleEntry {
                scale: "radius".into(),
                index: 1,
            })
            .expect("derived radius entry provenance");
        assert_eq!(scale_entry.applied_rule, "compiler.generators.radius_scale");
        assert_eq!(scale_entry.value_origin_rule, winning_rule);
        assert_eq!(scale_entry.token_path, ["dictionary.metrics.radius"]);
        assert_eq!(
            scale_entry.authored_metric,
            Some(crate::AuthoredMetric::Px { value: 8.0 })
        );

        let metric = success
            .candidate
            .provenance()
            .value(&DesignValueId::Metric("radius.md".into()))
            .expect("derived radius step provenance");
        assert_eq!(
            metric.applied_rule,
            "design.v1.primitives.metrics.radius.md"
        );
        assert_eq!(metric.value_origin_rule, "compiler.generators.radius_scale");
        assert_eq!(metric.token_path, ["dictionary.scales.radius[1]"]);
    }

    #[test]
    fn rejects_fractional_step_index_even_when_every_context_replaces_it() {
        let document = masked_padding_step_from_decimal("fractional-step", "0.5");
        assert_eq!(parsed_padding_step_value(&document), 0.5);
        assert_masked_padding_step_is_fatal(&document);
    }

    #[test]
    fn rejects_aliased_step_index_at_exactness_boundary_when_every_context_replaces_it() {
        let document = masked_padding_step_from_decimal("aliased-step", "9007199254740993");
        assert_eq!(
            parsed_padding_step_value(&document),
            9_007_199_254_740_992.0,
            "the authored alias must exercise the rounded boundary"
        );
        assert_masked_padding_step_is_fatal(&document);
    }

    #[test]
    fn rejects_representable_step_index_above_exactness_boundary_when_every_context_replaces_it() {
        let document =
            masked_padding_step_from_decimal("representable-high-step", "9007199254740994");
        assert_eq!(
            parsed_padding_step_value(&document),
            9_007_199_254_740_994.0
        );
        assert_masked_padding_step_is_fatal(&document);
    }

    #[test]
    fn parsed_double_alias_is_accepted_and_resolves_as_step_five() {
        // SPEC 19 §2.7: a source number denotes its parsed double, so this
        // lexeme is the integral step index 5 rather than an authored fraction.
        let document = padding_step_from_decimal("parsed-double-step-alias", "5.0000000000000001");
        assert_eq!(parsed_padding_step_value(&document), 5.0);

        let result = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!("parsed step alias was rejected: {:?}", diagnostics(&result));
        };
        let resolved = &success.candidate.dictionary().metrics["button.padding_x"];
        assert_eq!(resolved.value, 10.0);
        assert_eq!(
            success
                .candidate
                .provenance()
                .value(&DesignValueId::Metric("button.padding_x".into()))
                .expect("padding provenance")
                .authored_metric,
            Some(crate::AuthoredMetric::Step {
                scale: "spacing".into(),
                index: 5,
            })
        );
    }

    #[test]
    fn parsed_negative_zero_is_non_negative_and_resolves_as_zero() {
        // SPEC 19 §2.7 constrains the parsed double, not its spelling:
        // this negative underflow is zero and therefore non-negative.
        let source = EMBEDDED_DEFAULT_SOURCE.replace(
            "\"button.border_width\": { kind: \"px\", value: 1.0 }",
            "\"button.border_width\": { kind: \"px\", value: -1e-400 }",
        );
        let document = parse_design_source(SourceIdentity::new("parsed-negative-zero"), &source)
            .expect("negative-zero literal remains parseable");
        let MetricSource::Tagged(authored) = &document.v1.primitives.metrics["button.border_width"]
        else {
            panic!("fixture border width is tagged")
        };
        assert_eq!(authored.value, 0.0);
        assert!(authored.value.is_sign_negative());

        let result = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!(
                "parsed negative zero was rejected: {:?}",
                diagnostics(&result)
            );
        };
        assert_eq!(
            success.candidate.dictionary().metrics["button.border_width"].value,
            0.0
        );
    }

    #[test]
    fn rejects_non_px_scale_entry_without_producing_an_artifact() {
        let mut document = document();
        document
            .v1
            .primitives
            .scales
            .get_mut("type")
            .expect("type scale")[0] = MetricSource::ratio(11.333);
        document.v1.resolution_order = vec![ModifierAxis::Mode];
        let valid_type = || vec![MetricSource::px(11.333), MetricSource::px(13.333)];
        let mut light = block(ModifierAxis::Mode, "light");
        light.primitives.scales.insert("type".into(), valid_type());
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives.scales.insert("type".into(), valid_type());
        document.v1.modifiers = vec![light, dark];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let mismatch = diagnostics(&result)
            .iter()
            .find(|diagnostic| {
                diagnostic.code == "metric-kind-mismatch"
                    && diagnostic.path == "design.v1.primitives.scales.type[0]"
            })
            .expect("scale-entry kind mismatch");
        assert!(mismatch.message.contains("expects `px`"));
        assert!(mismatch.message.contains("is `ratio`"));
    }

    #[test]
    fn rejects_scale_field_on_px_and_ratio_metrics() {
        for kind in ["px", "ratio"] {
            let mut document = document();
            document.v1.primitives.metrics.insert(
                "button.height.md".into(),
                MetricSource::Tagged(TaggedMetricSource {
                    kind: kind.into(),
                    value: 28.0,
                    scale: Some("type".into()),
                }),
            );

            let result = compile_design(&document, DesignContext::default());
            assert!(matches!(result, DesignCompileResult::Fatal(_)));
            let invalid = diagnostics(&result)
                .iter()
                .find(|diagnostic| {
                    diagnostic.code == "invalid-metric"
                        && diagnostic.path == "design.v1.primitives.metrics.button.height.md"
                })
                .expect("illegal scale field diagnostic");
            assert!(
                invalid
                    .message
                    .contains(&format!("{kind} metrics cannot name a scale"))
            );
        }
    }

    #[test]
    fn shortened_modifier_scale_reports_only_the_affected_context() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson
            .primitives
            .scales
            .insert("type".into(), vec![MetricSource::px(11.333)]);
        document.v1.modifiers.push(crimson);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let failures = diagnostics(&result)
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "invalid-metric"
                    && diagnostic.message.contains("outside scale `type`")
            })
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1, "{:?}", diagnostics(&result));
        assert_eq!(
            failures[0].path,
            "design.v1[scheme=crimson].primitives.metrics.type.body"
        );
    }

    #[test]
    fn modifier_metric_kind_change_is_rejected_at_the_authoring_site() {
        for (name, replacement, base_kind, replacement_kind) in [
            ("button.height.md", MetricSource::ratio(28.0), "px", "ratio"),
            ("button.padding_x", MetricSource::px(42.0), "step", "px"),
        ] {
            let mut document = document();
            document.v1.resolution_order = vec![ModifierAxis::Scheme];
            let mut crimson = block(ModifierAxis::Scheme, "crimson");
            crimson.primitives.metrics.insert(name.into(), replacement);
            document.v1.modifiers.push(crimson);

            let result = compile_design(&document, DesignContext::default());
            assert!(matches!(result, DesignCompileResult::Fatal(_)));
            let failures = diagnostics(&result)
                .iter()
                .filter(|diagnostic| diagnostic.code == "modifier-metric-kind-change")
                .collect::<Vec<_>>();
            assert_eq!(failures.len(), 1, "{name}: {:?}", diagnostics(&result));
            assert_eq!(
                failures[0].path,
                format!("design.v1.modifiers[0].primitives.metrics.{name}")
            );
            assert!(
                failures[0]
                    .message
                    .contains(&format!("replacement is `{replacement_kind}`")),
                "{name}: {:?}",
                failures[0]
            );
            assert!(
                failures[0].message.contains(&format!(
                    "base metric `design.v1.primitives.metrics.{name}` is `{base_kind}`"
                )),
                "{name}: {:?}",
                failures[0]
            );
            assert!(
                diagnostics(&result)
                    .iter()
                    .all(|diagnostic| diagnostic.code != "metric-kind-mismatch"),
                "downstream position diagnostic obscured the source fault: {:?}",
                diagnostics(&result)
            );
        }
    }

    #[test]
    fn mapping_rules_may_select_a_metric_token_with_a_different_authored_kind() {
        let mut document = document();
        let mapping = document
            .v1
            .families
            .button
            .as_mut()
            .expect("button mapping");
        for rule in &mut mapping.variants {
            rule.set.remove(&ButtonProperty::Height);
        }
        mapping.variants[0].set.insert(
            ButtonProperty::Height,
            crate::MappingValueSource::Metric {
                value: "radius.md".into(),
            },
        );

        let result = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!(
                "rule-selected step metric should compile: {:?}",
                diagnostics(&result)
            );
        };
        let cell = success.candidate.tables().button.cell(ButtonCellKey {
            variant: ButtonVariant::Primary,
            size: ButtonSize::Md,
            interaction: InteractionState::Resting,
            focus_visible: false,
        });
        assert_eq!(cell.height, 4.0);
    }

    #[test]
    fn modifier_scale_entry_is_resolved_and_provenanced_through_the_metric() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson.primitives.scales.insert(
            "type".into(),
            vec![MetricSource::px(11.333), MetricSource::px(17.5)],
        );
        document.v1.modifiers.push(crimson);

        let result = compile_design(
            &document,
            DesignContext {
                scheme: Scheme::Crimson,
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(success) = result else {
            panic!("modifier scale should compile: {:?}", diagnostics(&result));
        };
        assert_eq!(success.candidate.dictionary().scales["type"][1], 17.5);
        assert_eq!(
            success.candidate.dictionary().metrics["type.body"].value,
            17.5
        );

        let scale_entry = success
            .candidate
            .provenance()
            .value(&DesignValueId::ScaleEntry {
                scale: "type".into(),
                index: 1,
            })
            .expect("scale-entry provenance");
        let modifier_rule = "design.v1.modifiers[0].primitives.scales.type[1]";
        assert_eq!(scale_entry.applied_rule, modifier_rule);
        assert_eq!(scale_entry.value_origin_rule, modifier_rule);

        let metric = success
            .candidate
            .provenance()
            .value(&DesignValueId::Metric("type.body".into()))
            .expect("step metric provenance");
        assert_eq!(
            metric.applied_rule,
            "design.v1.primitives.metrics.type.body"
        );
        assert_eq!(metric.value_origin_rule, modifier_rule);
        assert_eq!(metric.token_path, ["dictionary.scales.type[1]"]);
    }

    #[test]
    fn invalid_scale_does_not_relabel_authored_steps_as_unknown_metrics() {
        let mut document = document();
        document
            .v1
            .primitives
            .scales
            .get_mut("type")
            .expect("type scale")[0] = MetricSource::px(-1.0);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(diagnostics(&result).iter().any(|diagnostic| {
            diagnostic.code == "invalid-metric"
                && diagnostic.path == "design.v1.primitives.scales.type[0]"
        }));
        assert!(
            diagnostics(&result)
                .iter()
                .all(|diagnostic| diagnostic.code != "unknown-metric"),
            "false cascade remained: {:?}",
            diagnostics(&result)
        );
    }

    // The last unaliased integer below 2^53 remains admissible on a 64-bit target
    // and must reach the per-context range check without a saturating cast.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_largest_unaliased_step_index_is_reported_as_authored() {
        let mut document = document();
        document.v1.primitives.metrics.insert(
            "type.body".into(),
            MetricSource::step("type", 9_007_199_254_740_991.0),
        );

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(
            diagnostics(&result).iter().any(|diagnostic| {
                diagnostic.code == "invalid-metric"
                    && diagnostic
                        .message
                        .contains("step index 9007199254740991 is outside scale `type`")
            }),
            "a saturating cast would name a different index: {:?}",
            diagnostics(&result)
        );
    }

    #[test]
    fn unknown_metric_still_fires_for_a_name_that_was_never_authored() {
        // The companion to the test above: suppressing the cascade must not have
        // suppressed the genuine error. `button.height.md` is referenced by the
        // button base but no longer declared, so it is unauthored, not unresolved.
        let mut document = document();
        document
            .v1
            .primitives
            .metrics
            .remove("button.height.md")
            .expect("fixture declares button.height.md");

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(
            diagnostics(&result).iter().any(|diagnostic| {
                diagnostic.code == "unknown-metric"
                    && diagnostic.message.contains("button.height.md")
            }),
            "genuine unknown metric went unreported: {:?}",
            diagnostics(&result)
        );
    }

    #[test]
    fn rejects_a_scale_name_carrying_index_syntax_before_any_context_is_flattened() {
        // `type` and `type[0]` flatten to the same origin key, so a modifier
        // overriding `type` would repoint the *other* scale's diagnostic at itself
        // and split one invariant fault across contexts. Rejecting before flattening
        // is what keeps the reported path the authored one.
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        document
            .v1
            .primitives
            .scales
            .insert("type[0]".into(), vec![MetricSource::px(11.333)]);
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson.primitives.scales.insert(
            "type".into(),
            vec![MetricSource::px(11.333), MetricSource::px(17.5)],
        );
        document.v1.modifiers.push(crimson);

        let result = compile_design(
            &document,
            DesignContext {
                scheme: Scheme::Crimson,
                ..DesignContext::default()
            },
        );
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let reported = diagnostics(&result)
            .iter()
            .filter(|diagnostic| diagnostic.code == "invalid-scale-name")
            .collect::<Vec<_>>();
        assert_eq!(reported.len(), 1, "{:?}", diagnostics(&result));
        assert_eq!(reported[0].path, "design.v1.primitives.scales.type[0]");
    }

    #[test]
    fn rejects_a_modifier_authored_scale_name_carrying_index_syntax() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson
            .primitives
            .scales
            .insert("type[0]".into(), vec![MetricSource::px(11.333)]);
        document.v1.modifiers.push(crimson);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(
            diagnostics(&result).iter().any(|diagnostic| {
                diagnostic.code == "invalid-scale-name"
                    && diagnostic.path == "design.v1.modifiers[0].primitives.scales.type[0]"
            }),
            "{:?}",
            diagnostics(&result)
        );
    }

    #[test]
    fn ratio_cannot_become_a_fractional_pixel_button_height() {
        let mut document = document();
        document
            .v1
            .families
            .button
            .as_mut()
            .expect("button mapping")
            .base
            .insert(
                crate::ButtonProperty::Height,
                crate::MappingValueSource::Metric {
                    value: "lift.hover".into(),
                },
            );

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let mismatch = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "metric-kind-mismatch")
            .expect("ratio-to-height mismatch");
        assert!(mismatch.message.contains("expects `px`"));
        assert!(mismatch.message.contains("is `ratio`"));
    }

    #[test]
    fn lift_derivation_requires_a_ratio_metric() {
        let mut document = document();
        let mapping = document
            .v1
            .families
            .button
            .as_mut()
            .expect("button mapping");
        let crate::MappingValueSource::Derive { args, .. } = mapping.compound_variants[0]
            .set
            .get_mut(&crate::ButtonProperty::Pair)
            .expect("hover pair derivation")
        else {
            panic!("hover pair is not a derivation")
        };
        args[1] = crate::RecipeArgumentSource::Ratio {
            value: "button.border_width".into(),
        };
        assert_fatal_code(&document, "metric-kind-mismatch");
    }

    #[test]
    fn typography_type_step_requires_a_step_metric() {
        let mut document = document();
        document
            .v1
            .typography
            .records
            .get_mut("button.md")
            .expect("button typography")
            .type_step = "button.height.md".into();
        assert_fatal_code(&document, "metric-kind-mismatch");
    }

    #[test]
    fn rejects_claimed_axis_without_any_authored_block() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        assert_fatal_code(&document, "modifier-axis-claimed-without-block");
    }

    #[test]
    fn rejects_empty_modifier_when() {
        let mut document = document();
        document.v1.modifiers.push(compound_block(&[]));

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let empty = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-when-empty")
            .expect("empty-when diagnostic");
        assert_eq!(empty.path, "design.v1.modifiers[0].when");
    }

    #[test]
    fn rejects_duplicate_axis_in_resolution_order_at_repeat() {
        let mut document = document();
        document.v1.resolution_order = vec![
            ModifierAxis::Scheme,
            ModifierAxis::Mode,
            ModifierAxis::Scheme,
        ];
        document.v1.modifiers = vec![
            block(ModifierAxis::Scheme, "crimson"),
            block(ModifierAxis::Mode, "dark"),
        ];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let duplicate = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-axis-duplicate-in-resolution-order")
            .expect("duplicate-axis diagnostic");
        assert_eq!(duplicate.path, "design.v1.resolution_order[2]");
    }

    #[test]
    fn rejects_block_on_axis_absent_from_resolution_order() {
        let mut document = document();
        document
            .v1
            .modifiers
            .push(block(ModifierAxis::Scheme, "ocean"));
        let result = compile_design(&document, DesignContext::default());
        let diagnostic = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-axis-not-in-resolution-order")
            .expect("unclaimed-axis diagnostic");
        assert_eq!(diagnostic.path, "design.v1.modifiers[0].when.scheme");
        assert!(diagnostic.message.contains("scheme=ocean"));
    }

    #[test]
    fn rejects_unknown_fixed_axis_value() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        document
            .v1
            .modifiers
            .push(block(ModifierAxis::Scheme, "teal"));
        let result = compile_design(&document, DesignContext::default());
        let diagnostic = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-axis-value-unknown")
            .expect("unknown-value diagnostic");
        assert_eq!(diagnostic.path, "design.v1.modifiers[0].when.scheme");
        assert!(diagnostic.message.contains("teal"));
        assert!(diagnostic.message.contains("scheme"));
    }

    #[test]
    fn accepts_a_non_empty_app_axis_value_as_a_compile_time_selector() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::App];
        document.v1.modifiers.push(block(ModifierAxis::App, "mail"));
        let result = compile_design(
            &document,
            DesignContext {
                app: Some("mail".to_owned()),
                ..DesignContext::default()
            },
        );
        assert!(matches!(result, DesignCompileResult::Success(_)));
    }

    #[test]
    fn rejects_an_empty_app_axis_value() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::App];
        document.v1.modifiers.push(block(ModifierAxis::App, ""));
        assert_fatal_code(&document, "modifier-axis-value-unknown");
    }

    #[test]
    fn rejects_duplicate_when_block() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Mode];
        document.v1.modifiers = vec![
            block(ModifierAxis::Mode, "dark"),
            block(ModifierAxis::Mode, "dark"),
        ];
        assert_fatal_code(&document, "modifier-block-duplicate");
    }

    #[test]
    fn rejects_compatible_equal_key_blocks_writing_the_same_path() {
        let mut document = document();
        document.v1.resolution_order = vec![
            ModifierAxis::Scheme,
            ModifierAxis::Mode,
            ModifierAxis::Contrast,
        ];
        let mut scheme_contrast = compound_block(&[
            (ModifierAxis::Scheme, "crimson"),
            (ModifierAxis::Contrast, "high"),
        ]);
        scheme_contrast
            .primitives
            .metrics
            .insert("button.padding_x".into(), MetricSource::px(11.0));
        let mut mode_contrast = compound_block(&[
            (ModifierAxis::Mode, "dark"),
            (ModifierAxis::Contrast, "high"),
        ]);
        mode_contrast
            .primitives
            .metrics
            .insert("button.padding_x".into(), MetricSource::px(12.0));
        document.v1.modifiers = vec![scheme_contrast, mode_contrast];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let conflicts = diagnostics(&result)
            .iter()
            .filter(|diagnostic| diagnostic.code == "modifier-conflict")
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 1, "{:?}", diagnostics(&result));
        assert_eq!(
            conflicts[0].path,
            "design.v1.modifiers[1].primitives.metrics.button.padding_x"
        );
        assert!(conflicts[0].message.contains("design.v1.modifiers[0]"));
        assert!(conflicts[0].message.contains("design.v1.modifiers[1]"));
        assert!(
            conflicts[0]
                .message
                .contains("design.v1.primitives.metrics.button.padding_x")
        );
    }

    // Selector admission and context construction read the same closed
    // vocabularies from opposite ends. A value `context_value` can produce but
    // `from_name` cannot parse would make a whole scheme unauthorable, and the
    // symptom is a fatal `modifier-axis-value-unknown` on a value the compiler
    // itself emits — not something a fixture over today's six schemes notices.
    #[test]
    fn every_context_value_is_an_admissible_selector_value() {
        for scheme in Scheme::ALL {
            for mode in Mode::ALL {
                for contrast in Contrast::ALL {
                    let context = DesignContext {
                        scheme,
                        mode,
                        contrast,
                        app: None,
                    };
                    for axis in [
                        ModifierAxis::Scheme,
                        ModifierAxis::Mode,
                        ModifierAxis::Contrast,
                    ] {
                        let value = context_value(&context, axis);
                        assert_eq!(
                            classify_selector_value(axis, value),
                            SelectorAdmission::Selectable,
                            "{} value `{value}` is not an admissible selector",
                            axis_name(axis)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn ignores_conflicts_between_blocks_no_context_can_select() {
        let mut document = document();
        document.v1.resolution_order = vec![
            ModifierAxis::Scheme,
            ModifierAxis::Mode,
            ModifierAxis::Contrast,
        ];
        // `teal` is not a scheme, so this block is unselectable and its
        // overlap with the reachable one is not an ambiguity any context meets.
        let mut unselectable = compound_block(&[
            (ModifierAxis::Scheme, "teal"),
            (ModifierAxis::Contrast, "high"),
        ]);
        unselectable
            .primitives
            .metrics
            .insert("button.padding_x".into(), MetricSource::px(11.0));
        let mut reachable = compound_block(&[
            (ModifierAxis::Mode, "dark"),
            (ModifierAxis::Contrast, "high"),
        ]);
        reachable
            .primitives
            .metrics
            .insert("button.padding_x".into(), MetricSource::px(12.0));
        document.v1.modifiers = vec![unselectable, reachable];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let codes = diagnostics(&result)
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>();
        assert!(
            codes.contains(&"modifier-axis-value-unknown"),
            "{:?}",
            diagnostics(&result)
        );
        assert!(
            !codes.contains(&"modifier-conflict"),
            "unselectable block reported as a conflict: {:?}",
            diagnostics(&result)
        );
    }

    // Scales are replaced as whole vectors, so their collision key is the
    // scale name rather than an entry path. Metric-only fixtures would pass
    // with the scale registration in `modifier_token_paths` deleted, leaving
    // source order to pick the winning vector.
    #[test]
    fn rejects_compatible_equal_key_blocks_replacing_the_same_scale() {
        let mut document = document();
        document.v1.resolution_order = vec![
            ModifierAxis::Scheme,
            ModifierAxis::Mode,
            ModifierAxis::Contrast,
        ];
        let mut scheme_contrast = compound_block(&[
            (ModifierAxis::Scheme, "crimson"),
            (ModifierAxis::Contrast, "high"),
        ]);
        scheme_contrast.primitives.scales.insert(
            "type".into(),
            vec![MetricSource::px(12.0), MetricSource::px(14.0)],
        );
        let mut mode_contrast = compound_block(&[
            (ModifierAxis::Mode, "dark"),
            (ModifierAxis::Contrast, "high"),
        ]);
        mode_contrast
            .primitives
            .scales
            .insert("type".into(), vec![MetricSource::px(13.0)]);
        document.v1.modifiers = vec![scheme_contrast, mode_contrast];

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let conflicts = diagnostics(&result)
            .iter()
            .filter(|diagnostic| diagnostic.code == "modifier-conflict")
            .collect::<Vec<_>>();
        assert_eq!(conflicts.len(), 1, "{:?}", diagnostics(&result));
        assert_eq!(
            conflicts[0].path,
            "design.v1.modifiers[1].primitives.scales.type"
        );
        assert!(
            conflicts[0]
                .message
                .contains("design.v1.primitives.scales.type")
        );
    }

    #[test]
    fn compound_modifier_beats_each_single_axis_modifier_it_subsumes() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson
            .primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(11.0));
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(12.0));
        let mut crimson_dark = compound_block(&[
            (ModifierAxis::Scheme, "crimson"),
            (ModifierAxis::Mode, "dark"),
        ]);
        crimson_dark
            .primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(13.0));
        document.v1.modifiers = vec![crimson_dark, dark, crimson];

        for (scheme, mode, expected) in [
            (Scheme::Crimson, Mode::Light, 11.0),
            (Scheme::Ocean, Mode::Dark, 12.0),
            (Scheme::Crimson, Mode::Dark, 13.0),
        ] {
            let result = compile_design(
                &document,
                DesignContext {
                    scheme,
                    mode,
                    contrast: Contrast::Normal,
                    app: None,
                },
            );
            let DesignCompileResult::Success(success) = result else {
                panic!("{scheme:?}/{mode:?} failed: {:?}", diagnostics(&result));
            };
            assert_eq!(
                success.candidate.dictionary().metrics["button.border_width"].value,
                expected
            );
            if scheme == Scheme::Crimson && mode == Mode::Dark {
                let provenance = success
                    .candidate
                    .provenance()
                    .value(&DesignValueId::Metric("button.border_width".into()))
                    .expect("compound winner provenance");
                assert_eq!(
                    provenance.value_origin_rule,
                    "design.v1.modifiers[0].primitives.metrics.button.border_width"
                );
            }
        }
    }

    #[test]
    fn resolved_values_are_invariant_under_modifier_reordering_in_every_context() {
        let mut original = document();
        original.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson
            .primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(11.0));
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(12.0));
        let mut crimson_dark = compound_block(&[
            (ModifierAxis::Scheme, "crimson"),
            (ModifierAxis::Mode, "dark"),
        ]);
        crimson_dark
            .primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(13.0));
        // Scales ride along because they are replaced whole and delete their
        // indexed origin entries; a per-entry ordering fault would not show up
        // in a metric-only fixture.
        crimson.primitives.scales.insert(
            "type".into(),
            vec![MetricSource::px(11.0), MetricSource::px(13.0)],
        );
        crimson_dark.primitives.scales.insert(
            "type".into(),
            vec![MetricSource::px(12.0), MetricSource::px(14.0)],
        );
        original.v1.modifiers = vec![crimson, dark, crimson_dark];
        let mut reordered = original.clone();
        reordered.v1.modifiers.reverse();

        for scheme in Scheme::ALL {
            for mode in Mode::ALL {
                let context = DesignContext {
                    scheme,
                    mode,
                    contrast: Contrast::Normal,
                    app: None,
                };
                let original_result = compile_design(&original, context.clone());
                let reordered_result = compile_design(&reordered, context);
                let DesignCompileResult::Success(original_success) = original_result else {
                    panic!(
                        "original {scheme:?}/{mode:?} failed: {:?}",
                        diagnostics(&original_result)
                    );
                };
                let DesignCompileResult::Success(reordered_success) = reordered_result else {
                    panic!(
                        "reordered {scheme:?}/{mode:?} failed: {:?}",
                        diagnostics(&reordered_result)
                    );
                };
                // Provenance deliberately names authored array indexes, which
                // change under this shuffle. Compare every value-bearing part
                // of the artifact; those are the observable resolved design.
                assert_eq!(
                    original_success.candidate.tables(),
                    reordered_success.candidate.tables()
                );
                assert_eq!(
                    original_success.candidate.dictionary(),
                    reordered_success.candidate.dictionary()
                );
                assert_eq!(
                    original_success.candidate.typography(),
                    reordered_success.candidate.typography()
                );
            }
        }
    }

    #[test]
    fn rejects_family_or_typography_content_in_modifier() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut modifier = block(ModifierAxis::Scheme, "crimson");
        modifier.families = true;
        modifier.typography = true;
        document.v1.modifiers.push(modifier);
        assert_fatal_code(&document, "modifier-alters-family-mapping");
    }

    #[test]
    fn rejects_modifier_path_absent_from_base() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Contrast];
        let mut modifier = block(ModifierAxis::Contrast, "high");
        modifier
            .primitives
            .metrics
            .insert("new.metric".into(), MetricSource::px(8.0));
        document.v1.modifiers.push(modifier);
        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let diagnostic = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-introduces-unknown-path")
            .expect("unknown-path diagnostic");
        assert!(
            diagnostic
                .message
                .contains("declare it in the base source first")
        );
    }

    #[test]
    fn warns_when_modifier_restates_base_value() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut modifier = block(ModifierAxis::Scheme, "ocean");
        modifier.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 5.0),
        );
        document.v1.modifiers.push(modifier);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Success(_)));
        let diagnostic = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "modifier-restates-base")
            .expect("restatement warning");
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
        assert_eq!(
            diagnostic.path,
            "design.v1.modifiers[0].primitives.metrics.button.padding_x"
        );
    }

    #[test]
    fn requested_context_artifact_and_provenance_follow_winning_origins() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 9.0),
        );
        document.v1.modifiers.push(crimson);

        let crimson = compile_design(
            &document,
            DesignContext {
                scheme: Scheme::Crimson,
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(crimson) = crimson else {
            panic!("crimson context failed")
        };
        assert_eq!(
            crimson.candidate.dictionary().metrics["button.padding_x"].value,
            24.0
        );
        let overridden = crimson
            .candidate
            .provenance()
            .value(&DesignValueId::Metric("button.padding_x".into()))
            .expect("overridden metric provenance");
        assert_eq!(
            overridden.applied_rule,
            "design.v1.modifiers[0].primitives.metrics.button.padding_x"
        );
        assert_eq!(
            overridden.value_origin_rule,
            "design.v1.primitives.scales.spacing[9]"
        );
        assert_eq!(
            overridden.authored_metric,
            Some(crate::AuthoredMetric::Step {
                scale: "spacing".into(),
                index: 9,
            })
        );
        let base = crimson
            .candidate
            .provenance()
            .value(&DesignValueId::Metric("button.border_width".into()))
            .expect("base metric provenance");
        assert_eq!(
            base.value_origin_rule,
            "design.v1.primitives.metrics.button.border_width"
        );

        let ocean = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(ocean) = ocean else {
            panic!("ocean context failed")
        };
        assert_eq!(
            ocean.candidate.dictionary().metrics["button.padding_x"].value,
            10.0
        );
    }

    #[test]
    fn modifier_authored_value_diagnostic_names_both_context_and_modifier_path() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", f64::NAN),
        );
        document.v1.modifiers.push(crimson);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let failures = diagnostics(&result)
            .iter()
            .filter(|diagnostic| diagnostic.code == "invalid-metric")
            .collect::<Vec<_>>();
        // Whether caught structurally or by the winner compiler, the path must
        // retain the exact modifier authoring site.
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0]
                .path
                .ends_with(".modifiers[0].primitives.metrics.button.padding_x"),
            "{:?}",
            failures[0]
        );
    }

    #[test]
    fn all_context_mapping_warning_is_emitted_once_unqualified() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
        let mut crimson = block(ModifierAxis::Scheme, "crimson");
        crimson.primitives.metrics.insert(
            "button.padding_x".into(),
            MetricSource::step("spacing", 6.0),
        );
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(2.0));
        document.v1.modifiers = vec![crimson, dark];
        let mapping = document
            .v1
            .families
            .button
            .as_mut()
            .expect("button mapping");
        mapping.coverage = CoveragePolicy::Warn;
        mapping
            .compound_variants
            .retain(|rule| rule.when.interaction != Some(InteractionState::Pressed));

        let result = compile_design(&document, DesignContext::default());
        let DesignCompileResult::Success(success) = result else {
            panic!("coverage warning source should compile")
        };
        let warnings = success
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == "new-variant-uncovered" && diagnostic.message.contains("pressed")
            })
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].path, "design.v1.families.button");
    }

    #[test]
    fn app_axis_expansion_is_the_identity() {
        let context = DesignContext::default();
        assert_eq!(
            axis_contexts(context.clone(), ModifierAxis::App),
            vec![context]
        );
    }

    #[test]
    fn missing_requested_candidate_is_fatal_even_with_only_a_warning() {
        let document = document();
        let result = finish_compile(
            &document,
            None,
            vec![DesignDiagnostic::warning(
                "test-warning",
                "design.v1",
                "warning-only private-invariant fixture",
            )],
        );

        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(
            diagnostics(&result)
                .iter()
                .any(|diagnostic| diagnostic.code == "internal-requested-context-missing")
        );
    }

    #[test]
    fn total_composition_checks_all_twelve_scheme_mode_points() {
        let mut document = document();
        document.v1.resolution_order = vec![ModifierAxis::Scheme, ModifierAxis::Mode];
        for (scheme, min_width) in Scheme::ALL.into_iter().skip(1).zip(73..=77) {
            let mut modifier = block(ModifierAxis::Scheme, scheme.name());
            modifier.primitives.metrics.insert(
                "button.min_width.standard".into(),
                MetricSource::px(f64::from(min_width)),
            );
            document.v1.modifiers.push(modifier);
        }
        let mut dark = block(ModifierAxis::Mode, "dark");
        dark.primitives
            .metrics
            .insert("button.border_width".into(), MetricSource::px(2.0));
        document.v1.modifiers.push(dark);

        for (scheme_index, scheme) in Scheme::ALL.into_iter().enumerate() {
            for mode in Mode::ALL {
                let context = DesignContext {
                    scheme,
                    mode,
                    contrast: Contrast::Normal,
                    app: None,
                };
                let result = compile_design(&document, context);
                let DesignCompileResult::Success(success) = result else {
                    panic!("{scheme:?}/{mode:?} did not resolve: {result:#?}")
                };
                let expected_min_width = if scheme == Scheme::Ocean {
                    72.0
                } else {
                    72.0 + scheme_index as f64
                };
                assert_eq!(
                    success.candidate.dictionary().metrics["button.min_width.standard"].value,
                    expected_min_width
                );
                assert_eq!(
                    success.candidate.dictionary().metrics["button.border_width"].value,
                    if mode == Mode::Dark { 2.0 } else { 1.0 }
                );
            }
        }

        document
            .v1
            .primitives
            .colors
            .insert("product.surface".into(), colour(0.15));
        document
            .v1
            .primitives
            .colors
            .insert("product.foreground".into(), colour(0.95));
        document.v1.semantics.pairs.insert(
            "primary".into(),
            crate::PairSource::authored("product.surface", "product.foreground", None),
        );
        document.v1.modifiers[0]
            .primitives
            .colors
            .insert("product.surface".into(), colour(0.35));
        document
            .v1
            .modifiers
            .last_mut()
            .expect("dark block")
            .primitives
            .colors
            .insert("product.foreground".into(), colour(0.65));

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let failures = diagnostics(&result)
            .iter()
            .filter(|diagnostic| diagnostic.code == "text-contrast")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].path,
            "design.v1[scheme=crimson,mode=dark].semantics.pairs.primary"
        );
    }
}
