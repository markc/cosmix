//! Adapter-side mapping of resolved design colours; no iced dependency leaks
//! into `cosmix-design`. These are provisional widget mappings, not additions
//! to the design compiler's closed family registry.

use cosmix_design::{LinearRgba, ResolvedColours, ResolvedDictionary, ResolvedMetricKind};
use iced::{Border, Color, widget::text_input};

use crate::MenuStyle;

/// Missing or invalid resolved dictionary entry. Never silently substitutes a
/// fallback for a partially compiled design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenError(pub &'static str);

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing or invalid design token: {}", self.0)
    }
}

impl std::error::Error for TokenError {}

/// Converts linear-light design colours to iced's encoded sRGB components.
pub fn colour(value: LinearRgba) -> Color {
    // Use the model's canonical transfer function and quantisation, including
    // alpha. Passing linear channels directly makes middle greys too dark.
    let [r, g, b, a] = value.to_srgba8();
    Color::from_rgba8(r, g, b, f32::from(a) / 255.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub surface: Color,
    pub text: Color,
    pub popover: Color,
    pub popover_text: Color,
    pub muted_surface: Color,
    pub muted_text: Color,
    pub selection: Color,
    pub selection_text: Color,
    pub border: Color,
    pub input: Color,
    pub ring: Color,
    pub radius: f32,
}

impl Tokens {
    pub fn from_colours(colours: &ResolvedColours) -> Result<Self, TokenError> {
        let pair = |name| colours.pairs.get(name).ok_or(TokenError(name));
        let non_text = |name| {
            colours
                .non_text
                .get(name)
                .map(|v| colour(v.value))
                .ok_or(TokenError(name))
        };
        let base = pair("base")?;
        let popover = pair("popover")?;
        let muted = pair("muted")?;
        let accent = pair("accent")?;
        Ok(Self {
            surface: colour(base.rendered_surface),
            text: colour(base.rendered_foreground),
            popover: colour(popover.rendered_surface),
            popover_text: colour(popover.rendered_foreground),
            muted_surface: colour(muted.rendered_surface),
            muted_text: colour(muted.rendered_foreground),
            selection: colour(accent.rendered_surface),
            selection_text: colour(accent.rendered_foreground),
            border: non_text("border")?,
            input: non_text("input")?,
            ring: non_text("ring")?,
            radius: 6.0,
        })
    }

    pub fn from_dictionary(dictionary: &ResolvedDictionary) -> Result<Self, TokenError> {
        let mut tokens = Self::from_colours(&dictionary.colours)?;
        let radius = dictionary
            .metrics
            .get("radius.md")
            .ok_or(TokenError("radius.md"))?;
        if radius.kind != ResolvedMetricKind::Px
            || !radius.value.is_finite()
            || radius.value < 0.0
            || radius.value > f32::MAX as f64
        {
            return Err(TokenError("radius.md"));
        }
        tokens.radius = radius.value as f32;
        Ok(tokens)
    }

    pub fn text_input(self, status: text_input::Status) -> text_input::Style {
        let disabled = matches!(status, text_input::Status::Disabled);
        text_input::Style {
            background: if disabled {
                self.muted_surface
            } else {
                self.surface
            }
            .into(),
            border: Border {
                color: match status {
                    text_input::Status::Focused { .. } => self.ring,
                    text_input::Status::Hovered => self.border,
                    _ => self.input,
                },
                width: 1.0,
                radius: self.radius.into(),
            },
            icon: self.muted_text,
            placeholder: self.muted_text,
            value: if disabled { self.muted_text } else { self.text },
            selection: self.selection,
        }
    }

    pub fn menu_style(self) -> MenuStyle {
        MenuStyle {
            background: self.popover,
            text: self.popover_text,
            disabled: self.muted_text,
            selected: self.selection,
            selected_text: self.selection_text,
            border: self.border,
            radius: self.radius,
            ..MenuStyle::default()
        }
    }
}

/// Standalone preview palette. Applications should use their resolved design.
impl Default for Tokens {
    fn default() -> Self {
        Self {
            surface: Color::from_rgb8(27, 29, 35),
            text: Color::from_rgb8(230, 234, 241),
            popover: Color::from_rgb8(32, 36, 45),
            popover_text: Color::from_rgb8(230, 234, 241),
            muted_surface: Color::from_rgb8(38, 43, 53),
            muted_text: Color::from_rgb8(155, 163, 177),
            selection: Color::from_rgb8(47, 85, 130),
            selection_text: Color::WHITE,
            border: Color::from_rgb8(80, 91, 109),
            input: Color::from_rgb8(66, 77, 95),
            ring: Color::from_rgb8(143, 184, 232),
            radius: 6.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_design::{ResolvedMetric, ResolvedNonTextColour, ResolvedPair};

    fn dictionary() -> ResolvedDictionary {
        let mut colours = ResolvedColours::default();
        for name in ["base", "popover", "muted", "accent"] {
            colours.pairs.insert(
                name.into(),
                ResolvedPair {
                    surface_name: name.into(),
                    surface: LinearRgba::WHITE,
                    foreground_name: name.into(),
                    foreground: LinearRgba::BLACK,
                    backdrop_name: None,
                    backdrop: None,
                    rendered_surface: LinearRgba::BLACK,
                    rendered_foreground: LinearRgba::WHITE,
                    contrast_ratio: 21.0,
                    recipe: None,
                },
            );
        }
        for name in ["border", "input", "ring"] {
            colours.non_text.insert(
                name.into(),
                ResolvedNonTextColour {
                    value_name: name.into(),
                    value: LinearRgba::WHITE,
                    adjacent: Default::default(),
                },
            );
        }
        ResolvedDictionary {
            colours,
            metrics: [(
                "radius.md".into(),
                ResolvedMetric {
                    kind: ResolvedMetricKind::Px,
                    value: 9.0,
                },
            )]
            .into(),
            scales: Default::default(),
        }
    }

    #[test]
    fn linear_conversion_preserves_alpha_and_encodes_grey() {
        let mapped = colour(LinearRgba {
            red: 0.5,
            green: 0.5,
            blue: 0.5,
            alpha: 0.25,
        });
        assert_eq!(mapped, Color::from_rgba8(188, 188, 188, 64.0 / 255.0));
    }

    #[test]
    fn mapping_uses_rendered_pairs_and_focus_token() {
        let tokens = Tokens::from_dictionary(&dictionary()).unwrap();
        assert_eq!(tokens.surface, Color::BLACK);
        assert_eq!(tokens.text, Color::WHITE);
        assert_eq!(tokens.radius, 9.0);
        assert_eq!(
            tokens
                .text_input(text_input::Status::Focused { is_hovered: false })
                .border
                .color,
            tokens.ring
        );
        assert_eq!(
            tokens.text_input(text_input::Status::Disabled).value,
            tokens.muted_text
        );
        assert_eq!(tokens.menu_style().selected_text, tokens.selection_text);
    }

    #[test]
    fn incomplete_or_wrong_unit_dictionary_is_rejected() {
        assert_eq!(
            Tokens::from_colours(&ResolvedColours::default()),
            Err(TokenError("base"))
        );
        let mut dictionary = dictionary();
        dictionary.metrics.get_mut("radius.md").unwrap().kind = ResolvedMetricKind::Ratio;
        assert_eq!(
            Tokens::from_dictionary(&dictionary),
            Err(TokenError("radius.md"))
        );
    }
}
