//! # cosmix-deco
//!
//! Server-side decoration theme engine for the CosMix desktop.
//!
//! This crate is the *data and math* layer of the compositor theme system:
//! chrome styles (`mac` / `win11` / `cosmix`), decoration tokens, layout and
//! hit-testing. It renders nothing and reads no files — cosmix-comp owns the
//! Bevy entities and the `.mix` config plumbing, and calls in here for every
//! decision about what the chrome looks like and what a pointer position means.
//!
//! ```
//! use cosmix_deco::{presets, ChromeLayout, ChromePart, ChromeStyle, Mode, Scheme, vec2};
//!
//! let theme = presets::resolve(ChromeStyle::Mac, Scheme::Ocean, Mode::Dark);
//! let layout = ChromeLayout::compute(&theme, vec2(800.0, 600.0));
//! assert!(matches!(layout.hit_test(vec2(400.0, 14.0)), ChromePart::TitlebarDrag));
//! ```
//!
//! Design doc: `THEME_SYSTEM.md` at the crate root.

pub mod geom;
pub mod layout;
pub mod presets;
pub mod theme;

pub use geom::{oklch, rect, vec2, Rect, Srgba, Vec2};
pub use layout::{ChromeLayout, ChromePart, DecoExtents, ResizeEdge};
pub use theme::{
    pt_to_px, ButtonCluster, ButtonColors, ButtonShape, ButtonSide, ButtonState, CaptionButton,
    ChromeStyle, DecoColors, DecoFontFamily, DecoFontWeight, DecoMetrics, DecoTheme, Focus,
    GlyphPolicy, Mode, Scheme, ShadowSpec, TitleAlign,
};
