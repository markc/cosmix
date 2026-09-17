//! Colours, from the same `cosmix-design` compile the Bevy arm's CTK theme
//! resolves: the embedded revision-1 default source in the Ocean/Dark context,
//! mapped into iced through `cosmix_iced_widgets::Tokens`.
//!
//! A fixed context, not the user's theme files, so every run and both arms
//! draw the same colours. A design that will not compile is fatal: the arm
//! would otherwise report a measurement of a surface nobody specified.

use cosmix_design::{DesignCompileResult, DesignContext, Mode, Scheme, SourceIdentity};
use cosmix_iced_widgets::Tokens;
use iced::theme::Palette;
use iced::{Color, Theme};

/// The scheme both arms fix themselves to (the Bevy arm's `Scheme::Ocean`,
/// `ThemeMode::Dark`).
const SCHEME: Scheme = Scheme::Ocean;
const MODE: Mode = Mode::Dark;

/// Compiles the embedded design in the bake-off's fixed context.
pub fn tokens() -> Result<Tokens, String> {
    let document = cosmix_design::parse_design_source(
        SourceIdentity::new("embedded:bench-mixer-iced"),
        cosmix_design::EMBEDDED_DEFAULT_SOURCE,
    )
    .map_err(|error| format!("the embedded design source does not parse: {error:?}"))?;
    let context = DesignContext {
        scheme: SCHEME,
        mode: MODE,
        ..Default::default()
    };
    let DesignCompileResult::Success(compiled) = cosmix_design::compile_design(&document, context)
    else {
        return Err(format!(
            "the embedded design does not compile for ({}, {})",
            SCHEME.name(),
            MODE.name()
        ));
    };
    Tokens::from_dictionary(compiled.candidate.dictionary())
        .map_err(|error| format!("the compiled design is missing a widget token: {error}"))
}

/// The iced theme for text and containers. The pro-audio controls take
/// `tokens.audio_style()` explicitly and do not read this.
pub fn iced_theme(tokens: Tokens) -> Theme {
    Theme::custom(
        "cosmix-bench",
        Palette {
            background: tokens.surface,
            text: tokens.text,
            primary: tokens.primary,
            success: tokens.primary,
            warning: tokens.selection,
            danger: tokens.destructive,
        },
    )
}

/// Background of a channel strip. The Bevy arm takes the channel panel from
/// the design's second background rung and the master's from the third
/// (`ctk.panel` = bg2, `ctk.master.panel` = bg3); the iced token set names its
/// raised surfaces instead, so `card` carries the channel strips and
/// `popover` the master's lift.
pub fn strip_background(tokens: Tokens, master: bool) -> Color {
    if master { tokens.popover } else { tokens.card }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_design_resolves_every_widget_token() {
        let tokens = tokens().expect("the embedded Ocean/Dark design compiles");
        // A dark scheme: the surface must be darker than the text on it,
        // which also proves the linear -> sRGB conversion did not invert.
        let luma = |c: Color| c.r + c.g + c.b;
        assert!(luma(tokens.surface) < luma(tokens.text));
        assert!(tokens.radius > 0.0);
        let audio = tokens.audio_style();
        assert_eq!(audio.background, tokens.card);
        assert_ne!(audio.meter_clip, audio.meter_low);
        // The master strip must be visibly lifted off the channel strips, as
        // CTK's bg3-over-bg2 is; if the design ever collapses those rungs the
        // arm needs a different pair, not a silent flat board.
        assert_ne!(
            strip_background(tokens, true),
            strip_background(tokens, false),
            "master/channel panels collapsed; surface={:?} card={:?} popover={:?} muted={:?}",
            tokens.surface,
            tokens.card,
            tokens.popover,
            tokens.muted_surface
        );
    }

    #[test]
    fn the_theme_carries_the_resolved_palette() {
        let tokens = tokens().unwrap();
        let theme = iced_theme(tokens);
        assert_eq!(theme.palette().background, tokens.surface);
        assert_eq!(theme.palette().text, tokens.text);
    }
}
