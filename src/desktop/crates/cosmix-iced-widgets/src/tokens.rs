//! Adapter-side mapping of resolved design colours; no iced dependency leaks
//! into `cosmix-design`. These are provisional widget mappings, not additions
//! to the design compiler's closed family registry.

use cosmix_design::{LinearRgba, ResolvedColours, ResolvedDictionary, ResolvedMetricKind};
use iced_core::{Border, Color};
use iced_widget::text_input;

use crate::{AudioStyle, MenuStyle};

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

/// Widget colours and radius in iced terms, taken from a resolved design.
/// Pair fields use the rendered (composited) values; `border`, `input` and
/// `ring` are the non-text colours of the same names.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub surface: Color,
    pub text: Color,
    pub popover: Color,
    pub popover_text: Color,
    pub card: Color,
    pub card_text: Color,
    pub primary: Color,
    pub primary_text: Color,
    pub destructive: Color,
    pub destructive_text: Color,
    pub muted_surface: Color,
    pub muted_text: Color,
    pub selection: Color,
    pub selection_text: Color,
    pub border: Color,
    pub input: Color,
    pub ring: Color,
    /// The design's background rungs in its own order,
    /// `palette.background.1` to `.3`: elevation levels, not brightness
    /// levels, so a dark scheme climbs away from black and a light scheme
    /// away from white. Semantic pairs can collapse onto one surface (in
    /// Ocean dark, base, card, popover and muted all resolve to the same
    /// near-black), so a panel, a strip and a master board drawn from pairs
    /// alone read as flat. Draw those boards from the rungs instead.
    pub backgrounds: [Color; BACKGROUND_RUNGS],
    pub radius: f32,
}

/// How many background rungs the design carries.
pub const BACKGROUND_RUNGS: usize = 3;

/// The primitive names of the rungs, in the design's order.
pub const BACKGROUND_NAMES: [&str; BACKGROUND_RUNGS] = [
    "palette.background.1",
    "palette.background.2",
    "palette.background.3",
];

impl Tokens {
    /// Maps the `base`, `popover`, `card`, `primary`, `destructive`, `muted`
    /// and `accent` pairs, the `border`, `input` and `ring` colours, and the
    /// `palette.background.1..3` primitives. Radius is 6 px.
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
        let card = pair("card")?;
        let primary = pair("primary")?;
        let destructive = pair("destructive")?;
        let mut backgrounds = [Color::BLACK; BACKGROUND_RUNGS];
        for (rung, name) in BACKGROUND_NAMES.iter().enumerate() {
            backgrounds[rung] = colours
                .primitives
                .get(*name)
                .copied()
                .map(colour)
                .ok_or(TokenError(name))?;
        }
        Ok(Self {
            surface: colour(base.rendered_surface),
            text: colour(base.rendered_foreground),
            popover: colour(popover.rendered_surface),
            popover_text: colour(popover.rendered_foreground),
            card: colour(card.rendered_surface),
            card_text: colour(card.rendered_foreground),
            primary: colour(primary.rendered_surface),
            primary_text: colour(primary.rendered_foreground),
            destructive: colour(destructive.rendered_surface),
            destructive_text: colour(destructive.rendered_foreground),
            muted_surface: colour(muted.rendered_surface),
            muted_text: colour(muted.rendered_foreground),
            selection: colour(accent.rendered_surface),
            selection_text: colour(accent.rendered_foreground),
            border: non_text("border")?,
            input: non_text("input")?,
            ring: non_text("ring")?,
            backgrounds,
            radius: 6.0,
        })
    }

    /// As `from_colours`, with the radius from the `radius.md` px metric.
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

    /// Style for `TextField::style` (or a plain iced `text_input`).
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

    /// Style for `Menu::style`, keeping the default row metrics.
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

    /// Background rung `rung`, clamped to the rungs that exist: 0 for a
    /// window or panel, 1 for a strip, 2 for a raised board such as a master
    /// strip.
    pub fn background(&self, rung: usize) -> Color {
        self.backgrounds[rung.min(BACKGROUND_RUNGS - 1)]
    }

    /// Style for the pro-audio controls and canvases. Meter zones run
    /// primary (below -12 dB), accent (to -3 dB), destructive (above).
    pub fn audio_style(self) -> AudioStyle {
        AudioStyle {
            // A strip sits one rung above the window.
            background: self.background(1),
            track: self.muted_surface,
            fill: self.primary,
            thumb: self.card_text,
            text: self.card_text,
            muted_text: self.muted_text,
            border: self.border,
            meter_low: self.primary,
            meter_high: self.selection,
            meter_clip: self.destructive,
            peak: self.card_text,
            active: self.selection,
            active_text: self.selection_text,
            alert: self.destructive,
            alert_text: self.destructive_text,
            grid: self.border,
            lane: self.muted_surface,
            note: self.primary,
            waveform: self.primary,
            playhead: self.ring,
            radius: self.radius.min(4.0),
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
            card: Color::from_rgb8(30, 33, 40),
            card_text: Color::from_rgb8(230, 234, 241),
            primary: Color::from_rgb8(64, 160, 110),
            primary_text: Color::WHITE,
            destructive: Color::from_rgb8(205, 64, 64),
            destructive_text: Color::WHITE,
            muted_surface: Color::from_rgb8(38, 43, 53),
            muted_text: Color::from_rgb8(155, 163, 177),
            selection: Color::from_rgb8(47, 85, 130),
            selection_text: Color::WHITE,
            border: Color::from_rgb8(80, 91, 109),
            input: Color::from_rgb8(66, 77, 95),
            ring: Color::from_rgb8(143, 184, 232),
            backgrounds: [
                Color::from_rgb8(18, 20, 25),
                Color::from_rgb8(30, 33, 40),
                Color::from_rgb8(44, 48, 58),
            ],
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
        for name in [
            "base",
            "popover",
            "muted",
            "accent",
            "card",
            "primary",
            "destructive",
        ] {
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
        for (rung, name) in BACKGROUND_NAMES.iter().enumerate() {
            let level = 0.05 + rung as f64 * 0.1;
            colours.primitives.insert(
                (*name).into(),
                LinearRgba {
                    red: level,
                    green: level,
                    blue: level,
                    alpha: 1.0,
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
        let audio = tokens.audio_style();
        assert_eq!(audio.meter_clip, tokens.destructive);
        assert_eq!(audio.background, tokens.card);
        assert_eq!(audio.radius, 4.0);
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
        // A design without the rungs is rejected, not silently flattened.
        let mut dictionary = dictionary;
        dictionary.metrics.get_mut("radius.md").unwrap().kind = ResolvedMetricKind::Px;
        dictionary.colours.primitives.remove(BACKGROUND_NAMES[1]);
        assert_eq!(
            Tokens::from_dictionary(&dictionary),
            Err(TokenError(BACKGROUND_NAMES[1]))
        );
    }

    #[test]
    fn rungs_keep_the_designs_order_and_the_accessor_clamps() {
        let tokens = Tokens::from_dictionary(&dictionary()).unwrap();
        // The fixture's rungs climb 0.05, 0.15, 0.25 in linear light.
        assert!(luminance(tokens.background(0)) < luminance(tokens.background(1)));
        assert!(luminance(tokens.background(1)) < luminance(tokens.background(2)));
        assert_eq!(tokens.background(0), tokens.backgrounds[0]);
        assert_eq!(
            tokens.background(9),
            tokens.backgrounds[BACKGROUND_RUNGS - 1]
        );
        // A strip is drawn from rung 1, not from a semantic pair.
        assert_eq!(tokens.audio_style().background, tokens.background(1));
        // The preview palette has three distinct rungs too.
        let preview = Tokens::default().backgrounds;
        assert!(luminance(preview[0]) < luminance(preview[1]));
        assert!(luminance(preview[1]) < luminance(preview[2]));
    }

    fn luminance(colour: Color) -> f32 {
        let [r, g, b, _] = colour.into_linear();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// The gap the bake-off hit: in several schemes the semantic pairs all
    /// resolve to the same surface, so a panel, a strip and a master board
    /// drawn from them are one flat colour. The rungs must stay distinct and
    /// ordered in every shipped scheme and mode.
    #[test]
    fn every_shipped_scheme_has_three_distinct_ordered_rungs() {
        let document = cosmix_design::parse_design_source(
            cosmix_design::SourceIdentity::new("embedded:iced-widgets-rungs"),
            cosmix_design::EMBEDDED_DEFAULT_SOURCE,
        )
        .expect("the embedded design source parses");
        let mut flat_pairs = 0;
        for scheme in cosmix_design::Scheme::ALL {
            for mode in [cosmix_design::Mode::Light, cosmix_design::Mode::Dark] {
                let context = cosmix_design::DesignContext {
                    scheme,
                    mode,
                    ..Default::default()
                };
                let cosmix_design::DesignCompileResult::Success(compiled) =
                    cosmix_design::compile_design(&document, context)
                else {
                    panic!("embedded design does not compile for {scheme:?}/{mode:?}");
                };
                let tokens = Tokens::from_dictionary(compiled.candidate.dictionary())
                    .expect("the shipped design carries every token the adapter maps");
                let rungs = tokens.backgrounds;
                let levels: Vec<f32> = rungs.iter().copied().map(luminance).collect();
                let label = format!("{scheme:?}/{mode:?}");
                for (a, b) in [(0, 1), (1, 2), (0, 2)] {
                    assert_ne!(rungs[a], rungs[b], "{label}: rungs {a} and {b} are equal");
                }
                // Dark schemes climb away from black, light schemes away from
                // white; either way the order is strict.
                let climbing = levels[0] < levels[1];
                assert_eq!(
                    climbing,
                    levels[1] < levels[2],
                    "{label}: rungs are not monotonic ({levels:?})"
                );
                assert_eq!(
                    climbing,
                    mode == cosmix_design::Mode::Dark,
                    "{label}: rungs run the wrong way ({levels:?})"
                );
                if tokens.surface == tokens.card && tokens.card == tokens.popover {
                    flat_pairs += 1;
                }
            }
        }
        // Not a requirement, just the evidence for why the rungs exist: at
        // least one shipped scheme collapses its pairs onto one surface.
        assert!(
            flat_pairs > 0,
            "no shipped scheme flattens its pairs; the rungs may no longer be needed"
        );
    }
}
