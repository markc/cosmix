//! SPEC 19 §1.4's frozen v0↔v1 equivalence relation.

use std::collections::BTreeSet;

use crate::{
    ButtonCellKey, ButtonTypographyKey, Contrast, DesignContext, DesignDiagnostic,
    DesignSourceDocument, Mode, ResolvedMetricKind, Scheme, UnstampedResolvedDesign,
    V0CrosswalkExpressionSource, V0MappingProperty, V0PairMember,
};

pub(crate) const COMPARED_FIELDS: [&str; 29] = [
    "surface",
    "panel",
    "master_panel",
    "track",
    "control",
    "control_active",
    "thumb",
    "meter_green",
    "meter_amber",
    "meter_red",
    "text",
    "text_dim",
    "border",
    "row_hover",
    "row_selected",
    "row_selected_text",
    "row_selected_text_dim",
    "scrim",
    "danger_surface",
    "control_gap",
    "corner_radius",
    "fader_width",
    "fader_height",
    "knob_size",
    "meter_width",
    "scheme",
    "mode",
    "typography.family",
    "typography.body_px",
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum LegacyValue<'a> {
    Colour(&'a str),
    Metric(f64),
    Text(&'a str),
}

#[derive(Clone, Debug, PartialEq)]
enum ResolvedCrosswalkValue {
    Colour([u8; 4]),
    Metric(f64),
    Text(String),
}

/// Checks the frozen field surface before any v1 compilation and returns the
/// one context v0 can denote. Structural faults are independent, so all are
/// reported in one pass.
pub(crate) fn validate_crosswalk_shape(
    document: &DesignSourceDocument,
) -> (Option<DesignContext>, Vec<DesignDiagnostic>) {
    let mut diagnostics = Vec::new();
    let compared = COMPARED_FIELDS.into_iter().collect::<BTreeSet<_>>();

    for field in COMPARED_FIELDS {
        if legacy_value(document, field).is_none() {
            diagnostics.push(DesignDiagnostic::error(
                "v0-compared-field-absent",
                field_path(field),
                format!("v0 compared field `{field}` must be explicitly authored"),
            ));
        }
        if !document.v1.v0_crosswalk.contains_key(field) {
            diagnostics.push(DesignDiagnostic::error(
                "v0-crosswalk-row-missing",
                format!("design.v1.v0_crosswalk.{field}"),
                format!("v0 crosswalk has no row for compared field `{field}`"),
            ));
        }
    }
    for field in document.v1.v0_crosswalk.keys() {
        if !compared.contains(field.as_str()) {
            diagnostics.push(DesignDiagnostic::error(
                "v0-crosswalk-field-outside-compared-set",
                format!("design.v1.v0_crosswalk.{field}"),
                format!("`{field}` is outside SPEC 19 §1.4's frozen compared field set"),
            ));
        }
    }

    let context = match (
        document
            .legacy
            .scheme
            .as_deref()
            .and_then(Scheme::from_name),
        document.legacy.mode.as_deref().and_then(Mode::from_name),
    ) {
        (Some(scheme), Some(mode)) => Some(DesignContext {
            scheme,
            mode,
            contrast: Contrast::Normal,
            app: None,
        }),
        _ if document.legacy.scheme.is_some() && document.legacy.mode.is_some() => {
            diagnostics.push(DesignDiagnostic::error(
                "v0-equivalence-drift",
                "scheme",
                "v0 scheme or mode does not name a selectable v1 modifier-axis value",
            ));
            None
        }
        _ => None,
    };

    (context, diagnostics)
}

/// Compares every source-authored row against the pinned resolved artifact.
pub(crate) fn validate_equivalence(
    document: &DesignSourceDocument,
    context: &DesignContext,
    candidate: &UnstampedResolvedDesign,
) -> Vec<DesignDiagnostic> {
    let mut diagnostics = Vec::new();
    for field in COMPARED_FIELDS {
        let Some(legacy) = legacy_value(document, field) else {
            continue;
        };
        let Some(expression) = document.v1.v0_crosswalk.get(field) else {
            continue;
        };
        if let Err(message) = validate_expression_field(field, expression) {
            diagnostics.push(DesignDiagnostic::error(
                "v0-crosswalk-invalid-expression",
                format!("design.v1.v0_crosswalk.{field}"),
                message,
            ));
            continue;
        }
        let resolved = match evaluate(expression, context, candidate) {
            Ok(value) => value,
            Err(message) => {
                diagnostics.push(DesignDiagnostic::error(
                    "v0-crosswalk-invalid-expression",
                    format!("design.v1.v0_crosswalk.{field}"),
                    message,
                ));
                continue;
            }
        };
        match compare(legacy, &resolved) {
            Ok(()) => {}
            Err(message) => diagnostics.push(DesignDiagnostic::error(
                "v0-equivalence-drift",
                field_path(field),
                format!("v0 field `{field}` differs from its crosswalk expression: {message}"),
            )),
        }
    }
    diagnostics
}

fn validate_expression_field(
    field: &str,
    expression: &V0CrosswalkExpressionSource,
) -> Result<(), String> {
    let required_axis = match field {
        "scheme" => Some(crate::ModifierAxis::Scheme),
        "mode" => Some(crate::ModifierAxis::Mode),
        _ => None,
    };
    match (required_axis, expression) {
        (Some(required), V0CrosswalkExpressionSource::ModifierAxisSelection { axis })
            if *axis == required =>
        {
            Ok(())
        }
        (Some(required), _) => Err(format!(
            "crosswalk row `{field}` must select the `{}` modifier axis",
            modifier_axis_name(required)
        )),
        (None, V0CrosswalkExpressionSource::ModifierAxisSelection { axis }) => Err(format!(
            "the `{}` modifier axis cannot satisfy crosswalk row `{field}`",
            modifier_axis_name(*axis)
        )),
        (None, _) => Ok(()),
    }
}

fn modifier_axis_name(axis: crate::ModifierAxis) -> &'static str {
    match axis {
        crate::ModifierAxis::Scheme => "scheme",
        crate::ModifierAxis::Mode => "mode",
        crate::ModifierAxis::Contrast => "contrast",
        crate::ModifierAxis::App => "app",
    }
}

fn legacy_value<'a>(document: &'a DesignSourceDocument, field: &str) -> Option<LegacyValue<'a>> {
    let legacy = &document.legacy;
    let colour = |value: Option<&'a String>| value.map(|value| LegacyValue::Colour(value));
    let metric = |value: Option<f64>| value.map(LegacyValue::Metric);
    match field {
        "surface" => colour(legacy.surface.as_ref()),
        "panel" => colour(legacy.panel.as_ref()),
        "master_panel" => colour(legacy.master_panel.as_ref()),
        "track" => colour(legacy.track.as_ref()),
        "control" => colour(legacy.control.as_ref()),
        "control_active" => colour(legacy.control_active.as_ref()),
        "thumb" => colour(legacy.thumb.as_ref()),
        "meter_green" => colour(legacy.meter_green.as_ref()),
        "meter_amber" => colour(legacy.meter_amber.as_ref()),
        "meter_red" => colour(legacy.meter_red.as_ref()),
        "text" => colour(legacy.text.as_ref()),
        "text_dim" => colour(legacy.text_dim.as_ref()),
        "border" => colour(legacy.border.as_ref()),
        "row_hover" => colour(legacy.row_hover.as_ref()),
        "row_selected" => colour(legacy.row_selected.as_ref()),
        "row_selected_text" => colour(legacy.row_selected_text.as_ref()),
        "row_selected_text_dim" => colour(legacy.row_selected_text_dim.as_ref()),
        "scrim" => colour(legacy.scrim.as_ref()),
        "danger_surface" => colour(legacy.danger_surface.as_ref()),
        "control_gap" => metric(legacy.control_gap),
        "corner_radius" => metric(legacy.corner_radius),
        "fader_width" => metric(legacy.fader_width),
        "fader_height" => metric(legacy.fader_height),
        "knob_size" => metric(legacy.knob_size),
        "meter_width" => metric(legacy.meter_width),
        "scheme" => legacy.scheme.as_deref().map(LegacyValue::Text),
        "mode" => legacy.mode.as_deref().map(LegacyValue::Text),
        "typography.family" => legacy
            .typography
            .as_ref()
            .and_then(|typography| typography.family.as_deref())
            .map(LegacyValue::Text),
        "typography.body_px" => legacy
            .typography
            .as_ref()
            .and_then(|typography| typography.body_px)
            .map(LegacyValue::Metric),
        _ => None,
    }
}

fn field_path(field: &str) -> String {
    field.to_owned()
}

fn evaluate(
    expression: &V0CrosswalkExpressionSource,
    context: &DesignContext,
    candidate: &UnstampedResolvedDesign,
) -> Result<ResolvedCrosswalkValue, String> {
    let dictionary = candidate.dictionary();
    match expression {
        V0CrosswalkExpressionSource::Token { value } => dictionary
            .colours
            .primitives
            .get(value)
            .copied()
            .or_else(|| {
                dictionary
                    .colours
                    .non_text
                    .get(value)
                    .map(|token| token.value)
            })
            .map(|colour| ResolvedCrosswalkValue::Colour(colour.to_srgba8()))
            .ok_or_else(|| format!("token `{value}` does not name a resolved colour token")),
        V0CrosswalkExpressionSource::Pair { value, member } => {
            let pair = dictionary
                .colours
                .pairs
                .get(value)
                .ok_or_else(|| format!("pair `{value}` does not exist"))?;
            // The rendered members, not the declared ones. A v0 consumer paints
            // its field as a flat colour, so the value the gate must certify is
            // the one that reaches the screen — a transparent pair's declared
            // surface is `#00000000`, which paints nothing like its composite.
            let colour = match member {
                V0PairMember::Surface => pair.rendered_surface,
                V0PairMember::Foreground => pair.rendered_foreground,
            };
            Ok(ResolvedCrosswalkValue::Colour(colour.to_srgba8()))
        }
        V0CrosswalkExpressionSource::Metric { value } => {
            let metric = dictionary
                .metrics
                .get(value)
                .ok_or_else(|| format!("metric `{value}` does not exist"))?;
            if metric.kind != ResolvedMetricKind::Px {
                return Err(format!("metric `{value}` is not a px metric"));
            }
            Ok(ResolvedCrosswalkValue::Metric(metric.value))
        }
        V0CrosswalkExpressionSource::ModifierAxisSelection { axis } => {
            let value = match axis {
                crate::ModifierAxis::Scheme => context.scheme.name(),
                crate::ModifierAxis::Mode => context.mode.name(),
                crate::ModifierAxis::Contrast => context.contrast.name(),
                crate::ModifierAxis::App => context.app.as_deref().unwrap_or(""),
            };
            Ok(ResolvedCrosswalkValue::Text(value.to_owned()))
        }
        V0CrosswalkExpressionSource::ComponentMappingCell {
            family,
            variant,
            size,
            interaction,
            focus_visible,
            part,
            property,
        } => {
            if family != "button" {
                return Err(format!("component family `{family}` is not registered"));
            }
            let cell = candidate.tables().button.cell(ButtonCellKey {
                variant: *variant,
                size: *size,
                interaction: *interaction,
                focus_visible: *focus_visible,
            });
            match property {
                V0MappingProperty::PairSurface => Ok(ResolvedCrosswalkValue::Colour(
                    cell.pair.rendered_surface.to_srgba8(),
                )),
                V0MappingProperty::PairForeground => Ok(ResolvedCrosswalkValue::Colour(
                    cell.pair.rendered_foreground.to_srgba8(),
                )),
                V0MappingProperty::Border => cell
                    .border
                    .map(|value| ResolvedCrosswalkValue::Colour(value.to_srgba8()))
                    .ok_or_else(|| "selected mapping cell has no border".to_owned()),
                V0MappingProperty::Ring => cell
                    .ring
                    .map(|value| ResolvedCrosswalkValue::Colour(value.to_srgba8()))
                    .ok_or_else(|| "selected mapping cell has no ring".to_owned()),
                V0MappingProperty::Height => Ok(ResolvedCrosswalkValue::Metric(cell.height)),
                V0MappingProperty::MinWidth => Ok(ResolvedCrosswalkValue::Metric(cell.min_width)),
                V0MappingProperty::PaddingX => Ok(ResolvedCrosswalkValue::Metric(cell.padding_x)),
                V0MappingProperty::BorderWidth => {
                    Ok(ResolvedCrosswalkValue::Metric(cell.border_width))
                }
                V0MappingProperty::Radius => Ok(ResolvedCrosswalkValue::Metric(cell.radius)),
                V0MappingProperty::TypographyFamily | V0MappingProperty::TypographyBodyPx => {
                    let part = part.ok_or_else(|| {
                        "a typography mapping-cell expression must name a component part".to_owned()
                    })?;
                    let record = candidate.typography().button(ButtonTypographyKey {
                        variant: *variant,
                        size: *size,
                        part,
                    });
                    match property {
                        V0MappingProperty::TypographyFamily => {
                            Ok(ResolvedCrosswalkValue::Text(record.record.family.clone()))
                        }
                        V0MappingProperty::TypographyBodyPx => {
                            Ok(ResolvedCrosswalkValue::Metric(record.record.font_size))
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }
}

fn compare(legacy: LegacyValue<'_>, resolved: &ResolvedCrosswalkValue) -> Result<(), String> {
    match (legacy, resolved) {
        (LegacyValue::Colour(authored), ResolvedCrosswalkValue::Colour(resolved)) => {
            let authored = crate::parse_legacy_v0_hex_colour(authored)?;
            if authored == *resolved {
                Ok(())
            } else {
                Err(format!("authored {authored:?}, resolved {resolved:?}"))
            }
        }
        (LegacyValue::Metric(authored), ResolvedCrosswalkValue::Metric(resolved)) => {
            if authored == *resolved {
                Ok(())
            } else {
                Err(format!("authored {authored}px, resolved {resolved}px"))
            }
        }
        (LegacyValue::Text(authored), ResolvedCrosswalkValue::Text(resolved)) => {
            if authored == resolved {
                Ok(())
            } else {
                Err(format!("authored {authored:?}, resolved {resolved:?}"))
            }
        }
        (authored, resolved) => Err(format!(
            "crosswalk expression has the wrong value kind for {authored:?}: {resolved:?}"
        )),
    }
}

/// Decode the hexadecimal colour forms accepted by the shipped v0 reader.
///
/// Kept public so the CTK reader can enforce decoder agreement without adding
/// Bevy to this dependency-free compiler crate.
pub fn parse_legacy_v0_hex_colour(value: &str) -> Result<[u8; 4], String> {
    // The reader treats the `#` as optional, so the gate must too — rejecting a
    // hashless value would fail a theme the shipped v0 reader loads fine.
    let digits = value.strip_prefix('#').unwrap_or(value);
    let nibbles = digits
        .chars()
        .map(|digit| {
            digit
                .to_digit(16)
                .map(|value| value as u8)
                .ok_or_else(|| format!("v0 colour {value:?} contains a non-hex digit"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let doubled = |nibble: u8| (nibble << 4) | nibble;
    let joined = |high: u8, low: u8| (high << 4) | low;
    match nibbles.as_slice() {
        [red, green, blue] => Ok([doubled(*red), doubled(*green), doubled(*blue), 255]),
        [red, green, blue, alpha] => Ok([
            doubled(*red),
            doubled(*green),
            doubled(*blue),
            doubled(*alpha),
        ]),
        [
            red_high,
            red_low,
            green_high,
            green_low,
            blue_high,
            blue_low,
        ] => Ok([
            joined(*red_high, *red_low),
            joined(*green_high, *green_low),
            joined(*blue_high, *blue_low),
            255,
        ]),
        [
            red_high,
            red_low,
            green_high,
            green_low,
            blue_high,
            blue_low,
            alpha_high,
            alpha_low,
        ] => Ok([
            joined(*red_high, *red_low),
            joined(*green_high, *green_low),
            joined(*blue_high, *blue_low),
            joined(*alpha_high, *alpha_low),
        ]),
        _ => Err(format!(
            "v0 colour {value:?} is not a #RGB, #RGBA, #RRGGBB or #RRGGBBAA value"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ColourSpace, DesignCompileResult, EMBEDDED_DEFAULT_SOURCE, ModifierAxis,
        ModifierBlockSource, OklchSource, PrimitiveSource, SemanticSource, SourceIdentity,
        compile_design, parse_design_source,
    };

    fn document(identity: &'static str) -> DesignSourceDocument {
        parse_design_source(SourceIdentity::new(identity), EMBEDDED_DEFAULT_SOURCE)
            .expect("embedded equivalence fixture parses")
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
            "missing {code}: {:#?}",
            diagnostics(&result)
        );
    }

    fn modifier(axis: ModifierAxis, value: &str, colour: OklchSource) -> ModifierBlockSource {
        ModifierBlockSource {
            when: [(axis, value.to_owned())].into_iter().collect(),
            primitives: PrimitiveSource {
                colors: [("status.success".to_owned(), colour)]
                    .into_iter()
                    .collect(),
                metrics: Default::default(),
                scales: Default::default(),
            },
            semantics: SemanticSource::default(),
            families: false,
            typography: false,
        }
    }

    fn neutral(lightness: f64) -> OklchSource {
        OklchSource {
            color_space: ColourSpace::Oklch,
            l: lightness,
            c: 0.0,
            h: 0.0,
            alpha: 1.0,
        }
    }

    #[test]
    fn compared_field_set_has_28_top_level_fields_and_29_leaf_rows() {
        assert_eq!(COMPARED_FIELDS.len(), 29);
        assert_eq!(
            COMPARED_FIELDS
                .iter()
                .filter(|field| !field.starts_with("typography."))
                .count()
                + 1,
            28
        );
    }

    #[test]
    fn legacy_hex_parser_accepts_every_v0_reader_form() {
        assert_eq!(
            parse_legacy_v0_hex_colour("#123").unwrap(),
            [0x11, 0x22, 0x33, 255]
        );
        assert_eq!(
            parse_legacy_v0_hex_colour("#1234").unwrap(),
            [0x11, 0x22, 0x33, 0x44]
        );
        assert_eq!(
            parse_legacy_v0_hex_colour("#1234ab").unwrap(),
            [0x12, 0x34, 0xab, 255]
        );
        assert_eq!(
            parse_legacy_v0_hex_colour("#1234ab80").unwrap(),
            [0x12, 0x34, 0xab, 0x80]
        );
        assert!(parse_legacy_v0_hex_colour("red").is_err());
        assert!(parse_legacy_v0_hex_colour("#12345").is_err());
    }

    #[test]
    fn malformed_non_ascii_legacy_colour_is_a_diagnostic_not_a_panic() {
        let mut document = document("equivalence:non-ascii-colour");
        document.legacy.surface = Some("#0é000".to_owned());
        assert_fatal_code(&document, "v0-equivalence-drift");
    }

    #[test]
    fn embedded_fixture_covers_every_frozen_crosswalk_row() {
        let document = document("equivalence:all-rows");
        assert_eq!(
            document
                .v1
                .v0_crosswalk
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            COMPARED_FIELDS.into_iter().collect()
        );
        let result = compile_design(&document, DesignContext::default());
        assert!(
            matches!(result, DesignCompileResult::Success(_)),
            "{:#?}",
            diagnostics(&result)
        );
    }

    #[test]
    fn absent_compared_field_is_fatal_even_when_every_crosswalk_row_exists() {
        let mut document = document("equivalence:absent-field");
        document.legacy.surface = None;
        assert_fatal_code(&document, "v0-compared-field-absent");
    }

    #[test]
    fn gate_arms_for_absent_and_selection_only_v0_shapes() {
        let mut absent = document("equivalence:absent-v0-block");
        absent.legacy = Default::default();
        assert_fatal_code(&absent, "v0-compared-field-absent");

        let mut selection_only = document("equivalence:selection-only-v0-block");
        selection_only.legacy = crate::LegacyV0Source {
            scheme: Some("ocean".to_owned()),
            mode: Some("light".to_owned()),
            ..Default::default()
        };
        assert!(selection_only.legacy.is_selection_only());
        assert_fatal_code(&selection_only, "v0-compared-field-absent");
    }

    #[test]
    fn missing_crosswalk_row_is_fatal_even_when_the_v0_field_exists() {
        let mut document = document("equivalence:missing-row");
        document.v1.v0_crosswalk.remove("surface");
        assert_fatal_code(&document, "v0-crosswalk-row-missing");
    }

    #[test]
    fn crosswalk_row_outside_the_frozen_set_is_fatal() {
        let mut document = document("equivalence:outside-row");
        let expression = document.v1.v0_crosswalk["surface"].clone();
        document
            .v1
            .v0_crosswalk
            .insert("future_surface".to_owned(), expression);
        assert_fatal_code(&document, "v0-crosswalk-field-outside-compared-set");
    }

    #[test]
    fn byte_drift_is_fatal_after_the_v1_conversion_pipeline() {
        let mut document = document("equivalence:colour-drift");
        document.legacy.surface = Some("#f2fafc".to_owned());
        assert_fatal_code(&document, "v0-equivalence-drift");
    }

    #[test]
    fn metric_and_typography_tolerances_are_exact() {
        let mut metric = document("equivalence:metric-exact");
        metric.legacy.corner_radius = Some(5.0 + f64::EPSILON * 4.0);
        assert_fatal_code(&metric, "v0-equivalence-drift");

        let mut body = document("equivalence:body-px-exact");
        body.legacy.typography.as_mut().expect("typography").body_px =
            Some(13.333 + f64::EPSILON * 8.0);
        assert_fatal_code(&body, "v0-equivalence-drift");

        let mut family = document("equivalence:family-byte-equal");
        family
            .legacy
            .typography
            .as_mut()
            .expect("typography")
            .family = Some("Noto Sans ".to_owned());
        assert_fatal_code(&family, "v0-equivalence-drift");
    }

    #[test]
    fn axis_selection_expression_must_name_the_axis_owned_by_its_row() {
        let mut scheme = document("equivalence:scheme-axis-mismatch");
        scheme.v1.v0_crosswalk.insert(
            "scheme".to_owned(),
            V0CrosswalkExpressionSource::ModifierAxisSelection {
                axis: ModifierAxis::Mode,
            },
        );
        assert_fatal_code(&scheme, "v0-crosswalk-invalid-expression");

        let mut mode = document("equivalence:mode-axis-mismatch");
        mode.v1.v0_crosswalk.insert(
            "mode".to_owned(),
            V0CrosswalkExpressionSource::ModifierAxisSelection {
                axis: ModifierAxis::Scheme,
            },
        );
        assert_fatal_code(&mode, "v0-crosswalk-invalid-expression");
    }

    #[test]
    fn typography_row_cannot_be_satisfied_by_a_coincidentally_equal_axis() {
        let mut document = document("equivalence:typography-axis-coincidence");
        document
            .legacy
            .typography
            .as_mut()
            .expect("typography")
            .family = Some("ocean".to_owned());
        document.v1.v0_crosswalk.insert(
            "typography.family".to_owned(),
            V0CrosswalkExpressionSource::ModifierAxisSelection {
                axis: ModifierAxis::Scheme,
            },
        );
        assert_fatal_code(&document, "v0-crosswalk-invalid-expression");
    }

    #[test]
    fn scheme_row_cannot_be_satisfied_by_unrelated_typography() {
        let mut document = document("equivalence:scheme-typography-coincidence");
        document
            .v1
            .typography
            .records
            .get_mut("button.md")
            .expect("button.md typography")
            .family = "ocean".to_owned();
        document
            .legacy
            .typography
            .as_mut()
            .expect("typography")
            .family = Some("ocean".to_owned());
        let typography_expression = document.v1.v0_crosswalk["typography.family"].clone();
        document
            .v1
            .v0_crosswalk
            .insert("scheme".to_owned(), typography_expression);
        assert_fatal_code(&document, "v0-crosswalk-invalid-expression");
    }

    #[test]
    fn unresolved_crosswalk_expression_is_fatal() {
        let mut document = document("equivalence:bad-expression");
        document.v1.v0_crosswalk.insert(
            "surface".to_owned(),
            V0CrosswalkExpressionSource::Token {
                value: "missing.token".to_owned(),
            },
        );
        assert_fatal_code(&document, "v0-crosswalk-invalid-expression");
    }

    #[test]
    fn every_invalid_crosswalk_expression_shape_is_fatal() {
        let invalid = [
            V0CrosswalkExpressionSource::Pair {
                value: "missing.pair".to_owned(),
                member: V0PairMember::Surface,
            },
            V0CrosswalkExpressionSource::Metric {
                value: "missing.metric".to_owned(),
            },
            V0CrosswalkExpressionSource::Metric {
                value: "lift.hover".to_owned(),
            },
            V0CrosswalkExpressionSource::ComponentMappingCell {
                family: "unregistered".to_owned(),
                variant: Default::default(),
                size: Default::default(),
                interaction: Default::default(),
                focus_visible: false,
                part: None,
                property: V0MappingProperty::PairSurface,
            },
            V0CrosswalkExpressionSource::ComponentMappingCell {
                family: "button".to_owned(),
                variant: Default::default(),
                size: Default::default(),
                interaction: Default::default(),
                focus_visible: false,
                part: None,
                property: V0MappingProperty::Ring,
            },
            V0CrosswalkExpressionSource::ComponentMappingCell {
                family: "button".to_owned(),
                variant: Default::default(),
                size: Default::default(),
                interaction: Default::default(),
                focus_visible: false,
                part: None,
                property: V0MappingProperty::TypographyFamily,
            },
        ];
        for (index, expression) in invalid.into_iter().enumerate() {
            let mut document = document("equivalence:invalid-expression-shapes");
            document
                .v1
                .v0_crosswalk
                .insert("surface".to_owned(), expression);
            let result = compile_design(&document, DesignContext::default());
            assert!(
                diagnostics(&result)
                    .iter()
                    .any(|diagnostic| diagnostic.code == "v0-crosswalk-invalid-expression"),
                "case {index}: {:#?}",
                diagnostics(&result)
            );
        }
    }

    #[test]
    fn invalid_legacy_selection_cannot_choose_the_compared_context() {
        let mut document = document("equivalence:invalid-legacy-selection");
        document.legacy.scheme = Some("chartreuse".to_owned());
        let result = compile_design(&document, DesignContext::default());
        assert!(diagnostics(&result).iter().any(|diagnostic| {
            diagnostic.code == "v0-equivalence-drift"
                && diagnostic
                    .message
                    .contains("does not name a selectable v1 modifier-axis value")
        }));
    }

    #[test]
    fn crosswalk_expression_kind_must_match_the_legacy_field_kind() {
        let mut document = document("equivalence:wrong-expression-kind");
        document.v1.v0_crosswalk.insert(
            "surface".to_owned(),
            V0CrosswalkExpressionSource::Metric {
                value: "legacy.control_gap".to_owned(),
            },
        );
        assert_fatal_code(&document, "v0-equivalence-drift");
    }

    #[test]
    fn quantisation_fixture_covers_both_sides_of_one_byte_boundary() {
        for (lightness, expected) in [(0.501, "#636363"), (0.503, "#646464")] {
            let mut document = document("equivalence:quantisation-boundary");
            document
                .v1
                .primitives
                .colors
                .insert("status.success".to_owned(), neutral(lightness));
            document.legacy.meter_green = Some(expected.to_owned());
            let result = compile_design(&document, DesignContext::default());
            let DesignCompileResult::Success(success) = result else {
                panic!("{lightness}: {:#?}", diagnostics(&result));
            };
            assert_eq!(
                success.candidate.dictionary().colours.primitives["status.success"].to_srgba8(),
                parse_legacy_v0_hex_colour(expected).unwrap()
            );
        }
    }

    #[test]
    fn high_contrast_that_would_change_a_compared_field_is_excluded_from_the_gate() {
        let mut document = document("equivalence:high-contrast-pin");
        document.v1.resolution_order.push(ModifierAxis::Contrast);
        document
            .v1
            .modifiers
            .push(modifier(ModifierAxis::Contrast, "high", neutral(0.503)));
        let result = compile_design(
            &document,
            DesignContext {
                contrast: Contrast::High,
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(success) = result else {
            panic!("high-contrast source failed: {:#?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().colours.primitives["status.success"].to_srgba8(),
            [100, 100, 100, 255]
        );
        assert_ne!(document.legacy.meter_green.as_deref(), Some("#646464"));
    }

    #[test]
    fn app_overlay_that_would_change_a_compared_field_is_excluded_from_the_gate() {
        const FIXTURE_APP: &str = "dev.cosmix.equivalence-fixture";
        let mut document = document("equivalence:app-pin");
        document.v1.resolution_order.push(ModifierAxis::App);
        document
            .v1
            .modifiers
            .push(modifier(ModifierAxis::App, FIXTURE_APP, neutral(0.503)));
        let result = compile_design(
            &document,
            DesignContext {
                app: Some(FIXTURE_APP.to_owned()),
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(success) = result else {
            panic!("per-app source failed: {:#?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().colours.primitives["status.success"].to_srgba8(),
            [100, 100, 100, 255]
        );
        assert_ne!(document.legacy.meter_green.as_deref(), Some("#646464"));
    }

    #[test]
    fn total_composition_checks_an_authored_app_not_requested_by_the_caller() {
        const FIXTURE_APP: &str = "dev.cosmix.unrequested-fixture";
        let mut document = document("equivalence:app-total-composition");
        document.v1.resolution_order.push(ModifierAxis::App);
        let mut app = modifier(ModifierAxis::App, FIXTURE_APP, neutral(0.503));
        app.primitives.colors.clear();
        app.primitives.colors.insert(
            "palette.foreground.default".to_owned(),
            OklchSource {
                color_space: ColourSpace::Oklch,
                l: 0.98,
                c: 0.008,
                h: 220.0,
                alpha: 1.0,
            },
        );
        document.v1.modifiers.push(app);

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        assert!(diagnostics(&result).iter().any(|diagnostic| {
            diagnostic.code == "text-contrast"
                && diagnostic
                    .path
                    .contains("app=dev.cosmix.unrequested-fixture")
        }));
    }

    #[test]
    fn requested_unoverlaid_app_still_receives_an_artifact() {
        let mut document = document("equivalence:requested-unoverlaid-app");
        document.v1.resolution_order.push(ModifierAxis::App);
        document.v1.modifiers.push(modifier(
            ModifierAxis::App,
            "dev.cosmix.other-app",
            neutral(0.503),
        ));
        let result = compile_design(
            &document,
            DesignContext {
                app: Some("dev.cosmix.requested-app".to_owned()),
                ..DesignContext::default()
            },
        );
        let DesignCompileResult::Success(success) = result else {
            panic!("requested app failed: {:#?}", diagnostics(&result));
        };
        assert_eq!(
            success.candidate.dictionary().colours.primitives["status.success"].to_srgba8(),
            parse_legacy_v0_hex_colour(
                document
                    .legacy
                    .meter_green
                    .as_deref()
                    .expect("v0 meter green")
            )
            .unwrap(),
            "an unrelated app overlay must not change the requested artifact"
        );
    }

    #[test]
    fn pinned_equivalence_compile_repoints_fault_to_selected_modifier() {
        let mut document = document("equivalence:pinned-origin");
        document.v1.modifiers[0].semantics.pairs.insert(
            "base".to_owned(),
            crate::PairSource::authored("missing.surface", "palette.foreground.default", None),
        );

        let result = compile_design(&document, DesignContext::default());
        assert!(matches!(result, DesignCompileResult::Fatal(_)));
        let diagnostic = diagnostics(&result)
            .iter()
            .find(|diagnostic| diagnostic.code == "unknown-primitive")
            .expect("pinned-context primitive diagnostic");
        assert_eq!(
            diagnostic.path,
            "design.v1.modifiers[0].semantics.pairs.base"
        );
    }

    /// A v0 consumer paints its fields flat, so the gate must compare the
    /// colour that reaches the screen. Ghost/resting is the live case: its
    /// declared surface is fully transparent (`#00000000`) while it renders as
    /// the base surface it sits on. Comparing the declared member would demand
    /// the author write a colour v0 would have painted as nothing, and would
    /// reject the one v0 actually showed.
    #[test]
    fn a_transparent_cell_is_compared_by_what_it_renders_not_what_it_declares() {
        let ghost_resting_surface = V0CrosswalkExpressionSource::ComponentMappingCell {
            family: "button".to_owned(),
            variant: crate::ButtonVariant::Ghost,
            size: crate::ButtonSize::Md,
            interaction: crate::InteractionState::Resting,
            focus_visible: false,
            part: None,
            property: V0MappingProperty::PairSurface,
        };

        let mut rendered = document("equivalence:ghost-rendered");
        rendered
            .v1
            .v0_crosswalk
            .insert("panel".to_owned(), ghost_resting_surface.clone());
        rendered.legacy.panel = Some("#f3fafc".to_owned());
        let result = compile_design(&rendered, DesignContext::default());
        assert!(
            matches!(result, DesignCompileResult::Success(_)),
            "the composited colour is the one v0 painted: {:#?}",
            diagnostics(&result)
        );

        let mut declared = document("equivalence:ghost-declared");
        declared
            .v1
            .v0_crosswalk
            .insert("panel".to_owned(), ghost_resting_surface);
        declared.legacy.panel = Some("#00000000".to_owned());
        assert_fatal_code(&declared, "v0-equivalence-drift");
    }

    /// The same rule for a directly named pair. `muted` is authored as a
    /// transparent surface over `palette.background.1`, so its declared and
    /// rendered members are the two different colours this asserts between.
    #[test]
    fn a_transparent_pair_is_compared_by_what_it_renders_not_what_it_declares() {
        let muted_surface = V0CrosswalkExpressionSource::Pair {
            value: "muted".to_owned(),
            member: V0PairMember::Surface,
        };

        let mut rendered = document("equivalence:muted-rendered");
        rendered
            .v1
            .v0_crosswalk
            .insert("track".to_owned(), muted_surface.clone());
        rendered.legacy.track = Some("#f3fafc".to_owned());
        let result = compile_design(&rendered, DesignContext::default());
        assert!(
            matches!(result, DesignCompileResult::Success(_)),
            "the composited colour is the one v0 painted: {:#?}",
            diagnostics(&result)
        );

        let mut declared = document("equivalence:muted-declared");
        declared
            .v1
            .v0_crosswalk
            .insert("track".to_owned(), muted_surface);
        declared.legacy.track = Some("#00000000".to_owned());
        assert_fatal_code(&declared, "v0-equivalence-drift");
    }
}
