//! Strict-data source envelope and schema-version dispatch.

use std::collections::BTreeMap;
use std::fmt;

use cosmix_mix::value::Value;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::{ButtonProperty, ButtonSize, ButtonVariant, InteractionState, SourceIdentity};

/// Latest supported source-document structure version. Recipe names and
/// signatures belong to the compiler registry and are versioned with the crate,
/// not with this document envelope.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 1;

/// Stable source-load failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesignSourceErrorCode {
    StrictData,
    MissingSchemaVersion,
    SchemaVersionNotInteger,
    UnsupportedSchemaVersion,
    VersionGap,
    InvalidVersionedSource,
}

/// Source error carrying an agent-readable stable code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesignSourceError {
    pub source: SourceIdentity,
    pub code: DesignSourceErrorCode,
    pub message: String,
}

impl DesignSourceError {
    fn new(
        source: &SourceIdentity,
        code: DesignSourceErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: source.clone(),
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for DesignSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {} ({:?})",
            self.source.as_str(),
            self.message,
            self.code
        )
    }
}

impl std::error::Error for DesignSourceError {}

/// v1 base document or compile-time overlay.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    #[default]
    Base,
    Overlay,
}

/// Modifier axes whose flattening order is source-authored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum ModifierAxis {
    Scheme,
    Mode,
    Contrast,
    App,
}

/// Versioned v1 body. Token and mapping sections are added by the compiler
/// commits; the envelope is deliberately strict from its first revision.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesignV1Source {
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub resolution_order: Vec<ModifierAxis>,
    #[serde(default)]
    pub modifiers: Vec<ModifierBlockSource>,
    #[serde(default)]
    pub primitives: PrimitiveSource,
    #[serde(default)]
    pub semantics: SemanticSource,
    #[serde(default)]
    pub typography: TypographySource,
    #[serde(default)]
    pub families: FamilyMappingsSource,
    #[serde(default)]
    pub v0_crosswalk: BTreeMap<String, V0CrosswalkExpressionSource>,
}

/// Which half of an atomic pair a legacy colour field compares with.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum V0PairMember {
    Surface,
    Foreground,
}

/// A typed value obtainable from one resolved button mapping coordinate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum V0MappingProperty {
    PairSurface,
    PairForeground,
    Border,
    Ring,
    Height,
    MinWidth,
    PaddingX,
    BorderWidth,
    Radius,
    TypographyFamily,
    TypographyBodyPx,
}

/// Source-authored §1.4 expression for one frozen v0 field.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum V0CrosswalkExpressionSource {
    Token {
        value: String,
    },
    Pair {
        value: String,
        member: V0PairMember,
    },
    ComponentMappingCell {
        family: String,
        variant: ButtonVariant,
        size: ButtonSize,
        #[serde(default)]
        interaction: InteractionState,
        #[serde(default)]
        focus_visible: bool,
        #[serde(default)]
        part: Option<crate::ButtonPart>,
        property: V0MappingProperty,
    },
    Metric {
        value: String,
    },
    ModifierAxisSelection {
        axis: ModifierAxis,
    },
}

/// A sparse primitive/semantic overlay selected by a conjunction of
/// modifier-axis values.
///
/// `families` and `typography` are presence markers rather than parsed
/// sections. Accepting their values here lets the compiler issue SPEC 19's
/// stable `modifier-alters-family-mapping` diagnostic for any authored shape.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModifierBlockSource {
    #[serde(deserialize_with = "deserialize_modifier_when")]
    pub when: BTreeMap<ModifierAxis, String>,
    #[serde(default)]
    pub primitives: PrimitiveSource,
    #[serde(default)]
    pub semantics: SemanticSource,
    #[serde(default, deserialize_with = "present_ignored")]
    pub families: bool,
    #[serde(default, deserialize_with = "present_ignored")]
    pub typography: bool,
}

fn deserialize_modifier_when<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<ModifierAxis, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModifierWhenVisitor;

    impl<'de> Visitor<'de> for ModifierWhenVisitor {
        type Value = BTreeMap<ModifierAxis, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a modifier selector map")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            const AXES: &[&str] = &["scheme", "mode", "contrast", "app"];
            let mut when = BTreeMap::new();
            while let Some((authored_axis, value)) = map.next_entry::<String, String>()? {
                let axis = match authored_axis.as_str() {
                    "scheme" => ModifierAxis::Scheme,
                    "mode" => ModifierAxis::Mode,
                    "contrast" => ModifierAxis::Contrast,
                    "app" => ModifierAxis::App,
                    _ => return Err(serde::de::Error::unknown_field(&authored_axis, AXES)),
                };
                if when.insert(axis, value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate modifier axis `{authored_axis}` in `when`"
                    )));
                }
            }
            Ok(when)
        }
    }

    deserializer.deserialize_map(ModifierWhenVisitor)
}

fn present_ignored<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ = serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

/// Literal primitive values. Primitives cannot contain references.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimitiveSource {
    #[serde(default)]
    pub colors: BTreeMap<String, OklchSource>,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricSource>,
    #[serde(default)]
    pub scales: BTreeMap<String, Vec<MetricSource>>,
}

/// An authored metric. The untagged arm is retained so the compiler can issue
/// SPEC 19's stable `metric-untagged` diagnostic instead of collapsing the
/// failure into a generic source-deserialisation error.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum MetricSource {
    Tagged(TaggedMetricSource),
    Untagged(f64),
}

impl MetricSource {
    pub fn px(value: f64) -> Self {
        Self::Tagged(TaggedMetricSource {
            kind: "px".into(),
            value,
            scale: None,
        })
    }

    pub fn step(scale: impl Into<String>, index: f64) -> Self {
        Self::Tagged(TaggedMetricSource {
            kind: "step".into(),
            value: index,
            scale: Some(scale.into()),
        })
    }

    pub fn ratio(value: f64) -> Self {
        Self::Tagged(TaggedMetricSource {
            kind: "ratio".into(),
            value,
            scale: None,
        })
    }
}

/// Tagged metric record used in strict-data source.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaggedMetricSource {
    pub kind: String,
    pub value: f64,
    #[serde(default)]
    pub scale: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColourSpace {
    Oklch,
}

/// An explicitly annotated OKLCH authoring value.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OklchSource {
    pub color_space: ColourSpace,
    pub l: f64,
    pub c: f64,
    pub h: f64,
    #[serde(default = "opaque")]
    pub alpha: f64,
}

const fn opaque() -> f64 {
    1.0
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSource {
    #[serde(default)]
    pub pairs: BTreeMap<String, PairSource>,
    #[serde(default)]
    pub non_text: BTreeMap<String, NonTextColourSource>,
}

/// A text-bearing semantic pair, authored directly or produced by a
/// registered derivation. The untagged outer shape preserves the existing
/// `{ surface, foreground, backdrop? }` v1 record while giving derived pairs
/// an unambiguous `{ derive: { ... } }` form.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum PairSource {
    Authored(AuthoredPairSource),
    Derived { derive: DerivationCallSource },
}

/// A directly authored atomic surface/foreground pair.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthoredPairSource {
    pub surface: String,
    pub foreground: String,
    #[serde(default)]
    pub backdrop: Option<String>,
}

impl PairSource {
    pub fn authored(
        surface: impl Into<String>,
        foreground: impl Into<String>,
        backdrop: Option<String>,
    ) -> Self {
        Self::Authored(AuthoredPairSource {
            surface: surface.into(),
            foreground: foreground.into(),
            backdrop,
        })
    }
}

/// A lone colour with non-text adjacency obligations.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NonTextColourSource {
    pub value: String,
    #[serde(default)]
    pub adjacent: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypographySource {
    #[serde(default)]
    pub records: BTreeMap<String, TypeRecordSource>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeRecordSource {
    pub family: String,
    pub type_step: String,
    pub weight: u16,
    #[serde(default)]
    pub line_height: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyMappingsSource {
    #[serde(default)]
    pub button: Option<ButtonMappingSource>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoveragePolicy {
    #[default]
    Warn,
    Explicit,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonMappingSource {
    #[serde(default)]
    pub coverage: CoveragePolicy,
    #[serde(default)]
    pub base: BTreeMap<ButtonProperty, MappingValueSource>,
    #[serde(default)]
    pub variants: Vec<MappingRuleSource>,
    #[serde(default)]
    pub states: Vec<MappingRuleSource>,
    #[serde(default, rename = "compoundVariants")]
    pub compound_variants: Vec<MappingRuleSource>,
    #[serde(default)]
    pub inherit: ButtonInheritanceSource,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonInheritanceSource {
    #[serde(default)]
    pub variants: Vec<ButtonVariant>,
    #[serde(default)]
    pub sizes: Vec<ButtonSize>,
    #[serde(default)]
    pub interactions: Vec<InteractionState>,
    #[serde(default)]
    pub focus_visible: bool,
}

impl<'de> Deserialize<'de> for ButtonProperty {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "pair" => Ok(Self::Pair),
            "border" => Ok(Self::Border),
            "ring" => Ok(Self::Ring),
            "height" => Ok(Self::Height),
            "min_width" => Ok(Self::MinWidth),
            "padding_x" => Ok(Self::PaddingX),
            "border_width" => Ok(Self::BorderWidth),
            "radius" => Ok(Self::Radius),
            "typography" => Ok(Self::Typography),
            _ => Err(serde::de::Error::unknown_variant(
                &value,
                &[
                    "pair",
                    "border",
                    "ring",
                    "height",
                    "min_width",
                    "padding_x",
                    "border_width",
                    "radius",
                    "typography",
                ],
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MappingValueSource {
    Pair {
        value: String,
    },
    Token {
        value: String,
    },
    Metric {
        value: String,
    },
    Typography {
        value: String,
    },
    Derive {
        name: String,
        args: Vec<RecipeArgumentSource>,
    },
    Null,
}

/// A registered derivation call shared by semantic and mapping source sites.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivationCallSource {
    pub name: String,
    #[serde(default)]
    pub args: Vec<RecipeArgumentSource>,
}

/// A tagged authored argument. The tag is checked against the registry
/// signature; it is not inferred from positional context.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecipeArgumentSource {
    Pair { value: String },
    Colour { value: String },
    ColourList { values: Vec<String> },
    Ratio { value: String },
}

impl RecipeArgumentSource {
    pub const fn param(&self) -> crate::RecipeParam {
        match self {
            Self::Pair { .. } => crate::RecipeParam::Pair,
            Self::Colour { .. } => crate::RecipeParam::Colour,
            Self::ColourList { .. } => crate::RecipeParam::ColourList,
            Self::Ratio { .. } => crate::RecipeParam::Ratio,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingSelectorSource {
    #[serde(default)]
    pub variant: Option<ButtonVariant>,
    #[serde(default)]
    pub size: Option<ButtonSize>,
    #[serde(default)]
    pub interaction: Option<InteractionState>,
    #[serde(default)]
    pub focus_visible: Option<bool>,
    #[serde(default)]
    pub checked: Option<bool>,
    #[serde(default)]
    pub selected: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRuleSource {
    #[serde(default)]
    pub when: MappingSelectorSource,
    #[serde(default)]
    pub set: BTreeMap<ButtonProperty, MappingValueSource>,
}

/// Optional typography fields consumed by the shipped v0 reader.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LegacyTypographySource {
    pub family: Option<String>,
    pub body_px: Option<f64>,
}

/// The exact legacy top-level field surface. Unknown fields are intentionally
/// ignored here, demonstrating that the nested `design` section is isolated
/// from a v0-shaped reader.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct LegacyV0Source {
    pub scheme: Option<String>,
    pub mode: Option<String>,
    pub typography: Option<LegacyTypographySource>,
    pub surface: Option<String>,
    pub panel: Option<String>,
    pub master_panel: Option<String>,
    pub track: Option<String>,
    pub control: Option<String>,
    pub control_active: Option<String>,
    pub thumb: Option<String>,
    pub meter_green: Option<String>,
    pub meter_amber: Option<String>,
    pub meter_red: Option<String>,
    pub text: Option<String>,
    pub text_dim: Option<String>,
    pub border: Option<String>,
    pub row_hover: Option<String>,
    pub row_selected: Option<String>,
    pub row_selected_text: Option<String>,
    pub row_selected_text_dim: Option<String>,
    pub scrim: Option<String>,
    pub danger_surface: Option<String>,
    pub control_gap: Option<f64>,
    pub corner_radius: Option<f64>,
    pub fader_width: Option<f64>,
    pub fader_height: Option<f64>,
    pub knob_size: Option<f64>,
    pub meter_width: Option<f64>,
}

impl LegacyV0Source {
    /// True only for the grandfathered two-axis selection lane.
    pub fn is_selection_only(&self) -> bool {
        let Self {
            scheme: _,
            mode: _,
            typography,
            surface,
            panel,
            master_panel,
            track,
            control,
            control_active,
            thumb,
            meter_green,
            meter_amber,
            meter_red,
            text,
            text_dim,
            border,
            row_hover,
            row_selected,
            row_selected_text,
            row_selected_text_dim,
            scrim,
            danger_surface,
            control_gap,
            corner_radius,
            fader_width,
            fader_height,
            knob_size,
            meter_width,
        } = self;
        typography.is_none()
            && surface.is_none()
            && panel.is_none()
            && master_panel.is_none()
            && track.is_none()
            && control.is_none()
            && control_active.is_none()
            && thumb.is_none()
            && meter_green.is_none()
            && meter_amber.is_none()
            && meter_red.is_none()
            && text.is_none()
            && text_dim.is_none()
            && border.is_none()
            && row_hover.is_none()
            && row_selected.is_none()
            && row_selected_text.is_none()
            && row_selected_text_dim.is_none()
            && scrim.is_none()
            && danger_surface.is_none()
            && control_gap.is_none()
            && corner_radius.is_none()
            && fader_width.is_none()
            && fader_height.is_none()
            && knob_size.is_none()
            && meter_width.is_none()
    }
}

/// Parsed source with the legacy and v1 representations kept separate.
#[derive(Clone, Debug, PartialEq)]
pub struct DesignSourceDocument {
    pub identity: SourceIdentity,
    pub legacy: LegacyV0Source,
    pub v1: DesignV1Source,
}

/// Parse only the v0-shaped surface. The `design` section is ignored exactly
/// as it is by today's `ThemeFile` deserialiser.
pub fn parse_legacy_v0_source(source: &str) -> Result<LegacyV0Source, DesignSourceError> {
    cosmix_mix::from_conf_mix_str(source).map_err(|error| DesignSourceError {
        source: SourceIdentity::new("legacy-v0"),
        code: DesignSourceErrorCode::StrictData,
        message: error.to_string(),
    })
}

/// Parse strict-data, dispatch on the integer schema version, and only then
/// deserialize the v1 section. Unsupported content is never interpreted under
/// older rules.
pub fn parse_design_source(
    identity: SourceIdentity,
    source: &str,
) -> Result<DesignSourceDocument, DesignSourceError> {
    let value = cosmix_mix::parse_data(source).map_err(|error| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::StrictData,
            error.to_string(),
        )
    })?;
    let top = value_map(&value).ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            "design source must be a top-level map",
        )
    })?;
    let design = top.get("design").ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::MissingSchemaVersion,
            "missing design.schema_version",
        )
    })?;
    let design = value_map(design).ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            "design must be a map",
        )
    })?;
    let version = design.get("schema_version").ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::MissingSchemaVersion,
            "missing design.schema_version",
        )
    })?;
    let version = exact_schema_version(version).ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::SchemaVersionNotInteger,
            "design.schema_version must be an integer",
        )
    })?;
    if version > SUPPORTED_SCHEMA_VERSION {
        return Err(DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::VersionGap,
            format!(
                "schema version {version} is newer than supported version {SUPPORTED_SCHEMA_VERSION}"
            ),
        ));
    }
    if version != SUPPORTED_SCHEMA_VERSION {
        return Err(DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::UnsupportedSchemaVersion,
            format!("unsupported schema version {version}"),
        ));
    }

    let allowed_top_level = [
        "scheme",
        "mode",
        "typography",
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
        "design",
    ];
    if let Some(unknown) = top
        .keys()
        .find(|key| !allowed_top_level.contains(&key.as_str()))
    {
        return Err(DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            format!("unknown top-level field {unknown:?}"),
        ));
    }
    if let Some(unknown) = design
        .keys()
        .find(|key| !["schema_version", "v1"].contains(&key.as_str()))
    {
        return Err(DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            format!("unknown design envelope field {unknown:?}"),
        ));
    }
    let v1_value = design.get("v1").ok_or_else(|| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            "missing design.v1 section",
        )
    })?;
    let v1 = cosmix_mix::from_value(v1_value).map_err(|error| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            error.to_string(),
        )
    })?;
    let legacy = cosmix_mix::from_value(&value).map_err(|error| {
        DesignSourceError::new(
            &identity,
            DesignSourceErrorCode::InvalidVersionedSource,
            error.to_string(),
        )
    })?;
    Ok(DesignSourceDocument {
        identity,
        legacy,
        v1,
    })
}

fn value_map(value: &Value) -> Option<&cosmix_mix::IndexMap<String, Value>> {
    let Value::Map(map) = value else {
        return None;
    };
    Some(map)
}

fn exact_schema_version(value: &Value) -> Option<i64> {
    let Value::Number(number) = value else {
        return None;
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || *number < i64::MIN as f64
        || *number > i64::MAX as f64
    {
        return None;
    }
    Some(*number as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
scheme: "ocean"
mode: "dark"
design: {
  schema_version: 1,
  "v1": {
    kind: "base",
    resolution_order: ["scheme", "mode", "contrast", "app"]
  }
}
"#;

    fn parse(source: &str) -> Result<DesignSourceDocument, DesignSourceError> {
        parse_design_source(SourceIdentity::new("test"), source)
    }

    #[test]
    fn parses_v1_only_after_integer_version_dispatch() {
        let source = parse(MINIMAL).expect("valid versioned source");
        assert_eq!(source.legacy.scheme.as_deref(), Some("ocean"));
        assert_eq!(source.v1.kind, SourceKind::Base);
        assert_eq!(
            source.v1.resolution_order,
            [
                ModifierAxis::Scheme,
                ModifierAxis::Mode,
                ModifierAxis::Contrast,
                ModifierAxis::App,
            ]
        );
    }

    #[test]
    fn v0_reader_ignores_the_versioned_section() {
        let source = parse_legacy_v0_source(MINIMAL).expect("legacy parse");
        assert_eq!(source.scheme.as_deref(), Some("ocean"));
        assert_eq!(source.mode.as_deref(), Some("dark"));
        assert!(source.is_selection_only());
    }

    #[test]
    fn missing_schema_version_is_fatal() {
        let error = parse("scheme: \"ocean\"\n").unwrap_err();
        assert_eq!(error.code, DesignSourceErrorCode::MissingSchemaVersion);
    }

    #[test]
    fn non_integer_schema_versions_are_fatal() {
        for value in ["\"1\"", "1.5", "true"] {
            let error = parse(&format!(
                "design: {{ schema_version: {value}, \"v1\": {{ kind: \"base\" }} }}"
            ))
            .unwrap_err();
            assert_eq!(error.code, DesignSourceErrorCode::SchemaVersionNotInteger);
        }
    }

    #[test]
    fn zero_and_older_versions_are_not_v0() {
        for version in [0, -1] {
            let error = parse(&format!(
                "design: {{ schema_version: {version}, \"v1\": {{ kind: \"base\" }} }}"
            ))
            .unwrap_err();
            assert_eq!(error.code, DesignSourceErrorCode::UnsupportedSchemaVersion);
        }
    }

    #[test]
    fn only_versions_above_the_maximum_are_version_gaps() {
        let error = parse("design: { schema_version: 2, v2: {} }").unwrap_err();
        assert_eq!(error.code, DesignSourceErrorCode::VersionGap);
    }

    #[test]
    fn versioned_unknown_fields_are_rejected() {
        let source = MINIMAL.replace("kind: \"base\",", "kind: \"base\",\n    selector: \"*\",");
        let error = parse(&source).unwrap_err();
        assert_eq!(error.code, DesignSourceErrorCode::InvalidVersionedSource);
    }

    #[test]
    fn modifier_blocks_are_strict_but_retain_forbidden_section_presence() {
        let source = r#"
design: {
  schema_version: 1,
  "v1": {
    resolution_order: ["scheme", "mode"],
    modifiers: [{
      when: { scheme: "ocean", mode: "dark" },
      primitives: { metrics: { radius: 4.0 } },
      semantics: {},
      families: { arbitrary: ["shape"] },
      typography: "also retained as forbidden"
    }]
  }
}
"#;
        let parsed = parse(source).expect("modifier source parses");
        assert_eq!(
            parsed.v1.modifiers[0]
                .when
                .get(&ModifierAxis::Scheme)
                .map(String::as_str),
            Some("ocean")
        );
        assert_eq!(
            parsed.v1.modifiers[0]
                .when
                .get(&ModifierAxis::Mode)
                .map(String::as_str),
            Some("dark")
        );
        assert!(parsed.v1.modifiers[0].families);
        assert!(parsed.v1.modifiers[0].typography);

        let error =
            parse(&source.replace("semantics: {},", "semantics: {},\n      selector: \"*\","))
                .unwrap_err();
        assert_eq!(error.code, DesignSourceErrorCode::InvalidVersionedSource);
    }

    #[test]
    fn semantic_and_mapping_derivations_parse_tagged_arguments() {
        let source = parse(
            r#"
design: {
  schema_version: 1,
  "v1": {
    semantics: {
      pairs: {
        secondary: {
          derive: {
            name: "control_pair",
            args: [
              { kind: "colour", value: "palette.background.3" },
              { kind: "colour", value: "palette.accent.default" },
              { kind: "colour", value: "palette.background.1" },
              { kind: "colour", value: "palette.foreground.default" }
            ]
          }
        }
      }
    },
    families: {
      button: {
        base: {
          pair: {
            kind: "derive",
            name: "contrast_safe_state_pair",
            args: [
              { kind: "pair", value: "secondary" },
              { kind: "colour_list", values: ["palette.foreground.muted"] },
              { kind: "ratio", value: "lift.hover" }
            ]
          }
        }
      }
    }
  }
}
"#,
        )
        .expect("tagged recipe source parses");
        let PairSource::Derived { derive } = &source.v1.semantics.pairs["secondary"] else {
            panic!("secondary is not a semantic derivation")
        };
        assert_eq!(derive.args[0].param(), crate::RecipeParam::Colour);
        let MappingValueSource::Derive { args, .. } = source
            .v1
            .families
            .button
            .as_ref()
            .unwrap()
            .base
            .get(&ButtonProperty::Pair)
            .unwrap()
        else {
            panic!("button pair is not a mapping derivation")
        };
        assert_eq!(
            args.iter()
                .map(RecipeArgumentSource::param)
                .collect::<Vec<_>>(),
            [
                crate::RecipeParam::Pair,
                crate::RecipeParam::ColourList,
                crate::RecipeParam::Ratio,
            ]
        );
    }

    #[test]
    fn source_content_is_not_a_legacy_selection_record() {
        let source = parse_legacy_v0_source("scheme: \"ocean\"\ncontrol: \"#010203\"\n")
            .expect("legacy parse");
        assert!(!source.is_selection_only());
    }
}
