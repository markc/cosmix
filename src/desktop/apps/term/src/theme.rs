//! Every colour this frontend chooses, resolved from `cosmix-design`.
//!
//! Desktop-wide theming is mandatory (Mark 2026-09-18): a colour literal in an
//! app is a bug, including in a skeleton. There is exactly one literal-free
//! path here — compile the embedded default design for the app's context and
//! read the token dictionary — and one fallback, which is the SHARED preview
//! palette in `cosmix-iced-widgets`, not a local invention.
//!
//! What this does NOT colour is the grid. A terminal's cell colours belong to
//! the program running in it (rio's ANSI palette, resolved in
//! `cosmix_term_core::terminal::capture`); repainting those from the desktop
//! theme would make `ls --color` lie. The tokens own the window: the surface
//! behind and around the grid texture.

use cosmix_design::{DesignCompileResult, DesignContext, Mode, SourceIdentity};
use cosmix_iced_widgets::Tokens;

/// The app identity the design compiler selects a per-app overlay by.
const APP: &str = "term";

/// Resolved tokens for the terminal window.
///
/// A design that fails to compile is reported once and falls back to the
/// shared preview palette rather than killing a terminal over a colour — but
/// it is never silent, because a fleet running on the fallback palette looks
/// exactly like a fleet running on the design until someone reads the log.
pub fn tokens() -> Tokens {
    match resolve() {
        Ok(tokens) => tokens,
        Err(error) => {
            eprintln!("term theme: {error}; using the shared preview palette");
            Tokens::default()
        }
    }
}

fn resolve() -> Result<Tokens, String> {
    let document = cosmix_design::parse_design_source(
        SourceIdentity::new("embedded:cosmix-design-default"),
        cosmix_design::EMBEDDED_DEFAULT_SOURCE,
    )
    .map_err(|error| format!("embedded design source: {error:?}"))?;
    let context = DesignContext {
        mode: Mode::Dark,
        app: Some(APP.to_owned()),
        ..DesignContext::default()
    };
    let DesignCompileResult::Success(compiled) = cosmix_design::compile_design(&document, context)
    else {
        return Err("embedded design does not compile".into());
    };
    Tokens::from_dictionary(compiled.candidate.dictionary())
        .map_err(|error| format!("design dictionary: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded design must actually resolve, or every terminal silently
    /// runs on the preview palette and the theming rule is decoration.
    #[test]
    fn the_embedded_design_resolves_for_this_app() {
        let resolved = resolve().expect("the embedded design compiles for term");
        // Not a colour assertion — a contrast one. Surface and text coming
        // back equal would render an invisible window and still be "tokens".
        assert_ne!(resolved.surface, resolved.text);
        assert_ne!(resolved.surface, Tokens::default().surface);
    }
}
