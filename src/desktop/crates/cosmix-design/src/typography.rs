//! Desktop typography roles. Font discovery belongs to each rendering adapter.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "compiler", derive(serde::Deserialize))]
#[cfg_attr(feature = "compiler", serde(rename_all = "snake_case"))]
pub enum TypographyGeneric {
    #[default]
    SansSerif,
    Monospace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypographyRole {
    Ui,
    UiDisplay,
    Small,
    Mono,
    Terminal,
}

impl TypographyRole {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::UiDisplay => "ui_display",
            Self::Small => "small",
            Self::Mono => "mono",
            Self::Terminal => "terminal",
        }
    }
}

/// Prefer the active compiled role, with embedded defaults for omitted roles.
#[cfg(feature = "compiler")]
pub fn active_typography(
    typography: Option<&crate::ResolvedTypography>,
    role: TypographyRole,
) -> &crate::ResolvedTypeRecord {
    typography
        .and_then(|typography| typography.role(role))
        .unwrap_or_else(|| default_typography(role))
}

/// A Light request must not select an ExtraLight face in a fallback family.
/// Adapters inspect the selected family's faces (including variable ranges).
pub fn family_font_weight(requested: u16, has_light: bool) -> u16 {
    if requested == 300 && !has_light {
        400
    } else {
        requested
    }
}

/// Read the embedded role without compiling colours or widget tables. The
/// strict-data source is parsed once; there is no second Rust token authority.
#[cfg(feature = "compiler")]
pub fn default_typography(role: TypographyRole) -> &'static crate::ResolvedTypeRecord {
    use std::{collections::BTreeMap, sync::OnceLock};
    static ROLES: OnceLock<BTreeMap<String, crate::ResolvedTypeRecord>> = OnceLock::new();
    ROLES
        .get_or_init(|| {
            let source = crate::parse_design_source(
                crate::SourceIdentity::new("embedded-typography"),
                crate::EMBEDDED_DEFAULT_SOURCE,
            )
            .expect("valid embedded design source");
            source
                .v1
                .typography
                .records
                .into_iter()
                .filter_map(|(name, record)| {
                    let font_size = record.logical_px?;
                    Some((
                        name,
                        crate::ResolvedTypeRecord {
                            family: record.family,
                            fallbacks: record.fallbacks,
                            generic: record.generic,
                            font_size_metric: record.type_step,
                            font_size,
                            weight: record.weight,
                            line_height: record.line_height,
                        },
                    ))
                })
                .collect()
        })
        .get(role.name())
        .expect("embedded desktop typography role")
}
