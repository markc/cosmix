//! CTK additions to the upstream Feathers theme.
//!
//! Every colour a CTK widget paints comes from one of the [`tokens`] below —
//! the audio-console reskin is a palette edit applied through [`apply_theme`], never
//! a code sweep. Static colours bind via `ThemeBackgroundColor`/`ThemeTextColor`
//! components (feathers resolves them, live theme swaps included); dynamic
//! systems (meter level lanes, toggle lit-state) look tokens up through
//! `Res<UiTheme>` each time they repaint.

#[cfg(feature = "theme")]
use bevy::app::{AppExit, PreStartup};
use bevy::app::{PostUpdate, PropagateSet};
use bevy::color::Color;
use bevy::ecs::change_detection::DetectChangesMut;
use bevy::ecs::component::Component;
#[cfg(feature = "theme")]
use bevy::ecs::message::MessageWriter;
use bevy::ecs::message::{Message, MessageReader};
use bevy::ecs::query::{Changed, Has, Or, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::feathers::theme::{ThemeToken, UiTheme};
#[cfg(feature = "theme")]
use bevy::log::info;
use bevy::log::warn;
use bevy::prelude::IntoScheduleConfigs;
use bevy::prelude::{App, Entity, Plugin, Update};
use bevy::text::{
    detect_text_needs_rerender, FontCx, FontSize, FontSource, TextFont, TextPipeline,
};
use bevy::ui::UiSystems;
#[cfg(feature = "theme")]
use bevy::window::WindowFocused;
use std::collections::HashSet;

const AUTHORED_BODY_PX: f32 = 13.0;
const DEFAULT_BODY_PX: f32 = 13.333;
/// Bounds on the configured base size. Below the floor chrome is unreadable;
/// above the ceiling a single mistyped digit (`13333` for `13.333`) would ask
/// Bevy to rasterise glyph atlases thousands of pixels tall during startup.
/// Theme files are rejected outside this range with an explicit error, and
/// every other ingress — including a direct `apply_theme` with a hand-built
/// `TypographySpec` — is clamped where the value is consumed.
const MIN_BODY_PX: f32 = 6.0;
const MAX_BODY_PX: f32 = 96.0;
const DEFAULT_FONT_FAMILY: &str = "Noto Sans";

/// The CTK design-token vocabulary. Names are stable; values live in
/// [`ThemeSpec`] and are installed through [`apply_theme`].
pub mod tokens {
    use bevy::feathers::theme::ThemeToken;

    /// Window/background surface behind everything.
    pub const SURFACE: ThemeToken = ThemeToken::new_static("ctk.surface");
    /// Strip panel background.
    pub const PANEL: ThemeToken = ThemeToken::new_static("ctk.panel");
    /// The master strip's (faintly warmer) panel background.
    pub const MASTER_PANEL: ThemeToken = ThemeToken::new_static("ctk.master.panel");
    /// Fader/meter track wells.
    pub const TRACK: ThemeToken = ThemeToken::new_static("ctk.track");
    /// Resting control body (knob face, button base).
    pub const CONTROL: ThemeToken = ThemeToken::new_static("ctk.control");
    /// Engaged/active control accent (fader fill, knob pointer, lit toggle).
    pub const CONTROL_ACTIVE: ThemeToken = ThemeToken::new_static("ctk.control.active");
    /// Fader/scrubber thumb.
    pub const THUMB: ThemeToken = ThemeToken::new_static("ctk.thumb");
    /// Meter healthy level.
    pub const METER_GREEN: ThemeToken = ThemeToken::new_static("ctk.meter.green");
    /// Meter hot level.
    pub const METER_AMBER: ThemeToken = ThemeToken::new_static("ctk.meter.amber");
    /// Meter clip level / clip latch.
    pub const METER_RED: ThemeToken = ThemeToken::new_static("ctk.meter.red");
    /// Primary text.
    pub const TEXT: ThemeToken = ThemeToken::new_static("ctk.text");
    /// Secondary/dim text (captions, readouts, footers).
    pub const TEXT_DIM: ThemeToken = ThemeToken::new_static("ctk.text.dim");
    /// Subtle separator and inactive-focus border.
    pub const BORDER: ThemeToken = ThemeToken::new_static("ctk.border");
    /// Hovered list/tree row background.
    pub const ROW_HOVER: ThemeToken = ThemeToken::new_static("ctk.row.hover");
    /// Selected list/tree row background.
    pub const ROW_SELECTED: ThemeToken = ThemeToken::new_static("ctk.row.selected");
    /// Foreground — text and icons — on a selected row. Built-in palettes use
    /// the panel colour itself because their selected bar is separated far
    /// enough for that pure knockout to clear WCAG AA. Arbitrary overrides are
    /// only required to preserve the universal guarantee: AA against
    /// [`ROW_SELECTED`], not identity with [`PANEL`].
    pub const ROW_SELECTED_TEXT: ThemeToken = ThemeToken::new_static("ctk.row.selected.text");
    /// Dimmed metadata foreground on a selected row. Derived along the segment
    /// from [`ROW_SELECTED_TEXT`] toward [`ROW_SELECTED`], stopping at the
    /// furthest point that still clears WCAG AA.
    pub const ROW_SELECTED_TEXT_DIM: ThemeToken =
        ThemeToken::new_static("ctk.row.selected.text.dim");
    /// Modal backdrop which dims the application below it.
    pub const SCRIM: ThemeToken = ThemeToken::new_static("ctk.scrim");
    /// Low-emphasis destructive button surface.
    pub const DANGER_SURFACE: ThemeToken = ThemeToken::new_static("ctk.danger.surface");
}

/// Layout values deliberately kept separate from Feathers' colour-only theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadiusScale {
    pub sm: f32,
    pub md: f32,
    pub lg: f32,
    pub xl: f32,
}

impl RadiusScale {
    /// Derive the shadcn radius scale from its large/base radius.
    pub fn from_base(base: f32) -> Self {
        Self {
            sm: (base - 4.0).max(0.0),
            md: (base - 2.0).max(0.0),
            lg: base,
            xl: base + 4.0,
        }
    }
}

impl Default for RadiusScale {
    fn default() -> Self {
        Self::from_base(6.0)
    }
}

#[derive(Resource, Clone, Debug, PartialEq)]
pub struct CtkThemeMetrics {
    pub control_gap: f32,
    pub corner_radius: f32,
    pub radius: RadiusScale,
    #[deprecated(note = "CtkButton height is resolved from CtkDesign")]
    pub button_height: [f32; 3],
    #[deprecated(note = "CtkButton minimum width is resolved from CtkDesign")]
    pub button_min_width: f32,
    #[deprecated(note = "CtkButton horizontal padding is resolved from CtkDesign")]
    pub button_pad_h: f32,
    #[deprecated(note = "CtkButton border width is resolved from CtkDesign")]
    pub button_border: f32,
    pub fader_width: f32,
    pub fader_height: f32,
    pub knob_size: f32,
    pub meter_width: f32,
}

#[allow(deprecated)]
impl Default for CtkThemeMetrics {
    fn default() -> Self {
        Self {
            control_gap: 8.0,
            corner_radius: 5.0,
            radius: RadiusScale::default(),
            button_height: [24.0, 28.0, 32.0],
            button_min_width: 72.0,
            button_pad_h: 10.0,
            button_border: 1.0,
            fader_width: 42.0,
            fader_height: 250.0,
            knob_size: 58.0,
            meter_width: 28.0,
        }
    }
}

/// The typography values carried through the CTK theme cascade.
#[derive(Clone, Debug, PartialEq)]
pub struct TypographySpec {
    pub family: String,
    pub body_px: f32,
}

impl Default for TypographySpec {
    fn default() -> Self {
        Self {
            family: DEFAULT_FONT_FAMILY.to_string(),
            body_px: DEFAULT_BODY_PX,
        }
    }
}

/// The structured layer which supplied an effective typography value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TypographyProvenance {
    #[default]
    BuiltIn,
    SharedTheme,
    AppTheme,
    DirectApply,
    EmbeddedFallback,
}

/// Whether CTK is using the requested family or a safe fallback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TypographyFallback {
    Requested,
    LastKnownGood,
    #[default]
    Embedded,
}

/// Live, introspectable CTK typography state.
///
/// `effective_family == None` means CTK holds no font mapping: it stamps no
/// `TextFont` source, so text spawned in that state renders in whatever its
/// author asked for — Bevy/Feathers' embedded fallback for the usual entity
/// that named no font, and an explicit handle where one was given.
///
/// It is not an undo. Text CTK already stamped keeps the source it was given —
/// the embedded fallback is an ASCII-only subset, and unwinding to it because a
/// mapping was lost later would trade a working face for tofu. `body_px` is
/// unaffected either way: the configured size applies to every managed entity
/// whether or not the family resolves.
#[derive(Resource, Clone, Debug)]
pub struct CtkTypography {
    pub effective_family: Option<String>,
    pub requested_family: String,
    pub body_px: f32,
    pub revision: u64,
    pub fallback: TypographyFallback,
    pub family_provenance: TypographyProvenance,
    pub body_px_provenance: TypographyProvenance,
    pub last_warning: Option<String>,
    observed_theme_revision: Option<u64>,
    warned_families: HashSet<String>,
}

impl Default for CtkTypography {
    fn default() -> Self {
        Self {
            effective_family: None,
            requested_family: DEFAULT_FONT_FAMILY.to_string(),
            body_px: AUTHORED_BODY_PX,
            revision: 0,
            fallback: TypographyFallback::Embedded,
            family_provenance: TypographyProvenance::EmbeddedFallback,
            body_px_provenance: TypographyProvenance::EmbeddedFallback,
            last_warning: None,
            observed_theme_revision: None,
            warned_families: HashSet::new(),
        }
    }
}

/// Excludes one text entity from CTK's process-wide typography policy.
///
/// Code editors, icon fonts and deliberate display faces must add this marker
/// in the same spawn operation as their `TextFont`.
///
/// It is the only supported way to keep a bespoke `FontSource` on a text
/// entity: CTK owns the font of everything else it can see, and will restore
/// its own source to any managed entity that has been reassigned.
///
/// Removing the marker does not itself re-enrol the entity — CTK looks at a
/// text entity when its `TextFont` changes, and dropping a marker is not such a
/// change. The next write to `TextFont` re-enrols it, adopting whatever size is
/// written then as its authored size. Despawn and respawn if you want that to
/// happen at a moment you control.
///
/// One limitation applies to every managed entity, opted out or not: CTK
/// recognises an external size write **by value**, so writing exactly the size
/// CTK last applied is indistinguishable from CTK's own write and will not
/// become the new authored size. Write a different value, or opt out.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CtkTypographyOptOut;

#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct ManagedTypography {
    /// The size the spawn site asked for, at CTK's authoring baseline. Every
    /// effective size is derived from this, so repeated theme changes scale the
    /// original intent rather than compounding on the last result.
    authored_size: FontSize,
    /// The size CTK last observed on this entity — its own write where a
    /// mapping was available, otherwise whatever it read. A `font_size` that no
    /// longer matches is somebody else's write, and becomes the new authored
    /// size. Tracking the *observed* rather than the *written* size is what lets
    /// a write made before any mapping existed still be adopted.
    last_seen_size: FontSize,
}

/// Whether CTK may derive an effective size from this authored size.
///
/// A non-finite size cannot settle: the scaled result is also non-finite, and
/// `NaN != NaN` would report drift on every frame, rewriting the component and
/// re-triggering text layout forever. Such an entity is left exactly as its
/// author wrote it, and reconsidered if the size is later corrected.
fn is_manageable_size(size: FontSize) -> bool {
    let (FontSize::Px(v)
    | FontSize::Vw(v)
    | FontSize::Vh(v)
    | FontSize::VMin(v)
    | FontSize::VMax(v)
    | FontSize::Rem(v)) = size;
    v.is_finite()
}

type TypographyTextQueryData = (
    Entity,
    &'static mut TextFont,
    Option<&'static mut ManagedTypography>,
    Has<CtkTypographyOptOut>,
);
// `Changed` rather than `Added`: an entity CTK declined to manage on sight —
// one spawned with a size it cannot derive from — must get another look when
// that size is corrected, or it would stay unmanaged for the rest of its life.
type TypographyTextQueryFilter = Or<(Changed<TextFont>, With<ManagedTypography>)>;

/// An OKLCH colour — perceptual **L**ightness (as a percentage, to read
/// straight from the web CSS), **C**hroma, **H**ue (degrees). The cosmix
/// palette primitive, shared with the web design system (dcs.spa / webd
/// `site.css`): a scheme is a hue; L/C are tuned per role + mode. Bevy's
/// `Color` is OKLCH-native, so this maps 1:1.
#[derive(Clone, Copy, Debug)]
pub struct Oklch {
    pub l: f32,
    pub c: f32,
    pub h: f32,
}

impl Oklch {
    pub const fn new(l: f32, c: f32, h: f32) -> Self {
        Self { l, c, h }
    }
    pub fn color(self) -> Color {
        Color::oklch(self.l / 100.0, self.c, self.h)
    }
}

const fn ok(l: f32, c: f32, h: f32) -> Oklch {
    Oklch::new(l, c, h)
}

/// WCAG 2.1 AA for body text — the floor every derived foreground token in this
/// module clears, asserted for all six schemes in both modes.
pub const AA_CONTRAST: f32 = 4.5;

/// AA is enough for the knockout alone, but measured at 4.5 three of the twelve
/// scheme/mode palettes leave zero lightness headroom for a dimmed selected-row
/// foreground. A 7.0 separation leaves at least 13 lightness units everywhere.
/// This target comes from the palette probe, not an eyeballed preference.
const SELECTION_SEPARATION: f32 = 7.0;

/// WCAG 2.1 relative luminance of a colour. Alpha is ignored in the
/// calculation but, like RGB, must be finite.
///
/// A non-finite channel has no physical luminance, so it returns NaN rather
/// than being clamped into a plausible finite colour.
fn relative_luminance(color: Color) -> f32 {
    fn channel(c: f32) -> f32 {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let s = bevy::color::Srgba::from(color);
    if ![s.red, s.green, s.blue, s.alpha]
        .into_iter()
        .all(f32::is_finite)
    {
        return f32::NAN;
    }
    0.2126 * channel(s.red) + 0.7152 * channel(s.green) + 0.0722 * channel(s.blue)
}

/// WCAG 2.1 contrast ratio between two finite opaque colours, `1.0..=21.0`.
///
/// Public because legibility is not CTK's private business: an app deriving a
/// foreground of its own, or an agent writing a theme file, should be able to
/// check the same number this module checks.
///
/// Returns NaN if any channel, including alpha, is non-finite. That value
/// cannot be mistaken for a passing ratio by a `>=` threshold check.
///
/// **Alpha is ignored, and that is a precondition, not a detail.** A translucent
/// colour composites against whatever is underneath, which this cannot see, so
/// the number returned for one is not a contrast ratio at all: `#ffffff00` on
/// black measures a perfect 21:1 and paints nothing. Callers deriving or
/// checking a pairing must establish that both colours are opaque —
/// [`is_opaque`] is the check this module uses.
pub fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (a, b) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// Whether [`contrast_ratio`] is meaningful for this colour — see its docs.
pub fn is_opaque(color: Color) -> bool {
    let color = bevy::color::Srgba::from(color);
    [color.red, color.green, color.blue, color.alpha]
        .into_iter()
        .all(f32::is_finite)
        && color.alpha >= 1.0
}

fn has_finite_channels(color: Color) -> bool {
    let color = bevy::color::Srgba::from(color);
    [color.red, color.green, color.blue, color.alpha]
        .into_iter()
        .all(f32::is_finite)
}

/// Why either selected-row foreground does not honour its contrast guarantee.
///
/// Split out from the warning so the decision is a value a test can assert on.
/// Observing a `warn!` requires a log subscriber; observing this does not.
fn selection_pairing_fault(fg: Color, dim: Color, bg: Color) -> Option<String> {
    if !has_finite_channels(fg) || !has_finite_channels(dim) || !has_finite_channels(bg) {
        return Some(
            "theme override makes row_selected_text, row_selected_text_dim, or \
             row_selected contain a non-finite channel; contrast against it is \
             undefined and CTK's WCAG AA guarantee for selected rows does not hold"
                .to_string(),
        );
    }
    if !is_opaque(fg) || !is_opaque(dim) || !is_opaque(bg) {
        // Not a low ratio — no ratio. Reporting a number here would be worse
        // than reporting nothing, because it would be a passing one.
        return Some(
            "theme override makes row_selected_text, row_selected_text_dim, or \
             row_selected translucent; \
             contrast against it is undefined and CTK's WCAG AA guarantee for \
             selected rows does not hold"
                .to_string(),
        );
    }
    for (name, foreground) in [("row_selected_text", fg), ("row_selected_text_dim", dim)] {
        let measured = contrast_ratio(foreground, bg);
        // Written to fail closed. A non-finite channel — reachable through the
        // public `ThemeColors` — makes `measured` NaN, which must fail rather
        // than slip through the ordinary `<` comparison.
        #[expect(
            clippy::neg_cmp_op_on_partial_ord,
            reason = "the negation is the assertion: NaN must fail this comparison"
        )]
        if !(measured >= AA_CONTRAST) {
            if !measured.is_finite() {
                return Some(format!(
                    "{name} or row_selected has a non-finite channel; contrast \
                     against it is not a number ({measured}), so CTK's WCAG AA \
                     guarantee for selected rows does not hold"
                ));
            }
            return Some(format!(
                "theme override {name} measures {measured:.2}:1 against \
                 row_selected, below WCAG AA ({AA_CONTRAST}:1)"
            ));
        }
    }
    None
}

/// Report an unchecked selection pairing.
///
/// The override always stands: a theme file is the operator's call, and CTK
/// refusing a hex would be worse than a legible warning. But the whole point of
/// the token is that it is checked, so an unchecked value says so rather than
/// silently undoing the derivation.
///
/// Both file overlays and direct runtime [`ApplyTheme`] messages can carry an
/// unchecked public [`ThemeColors`], so every committed-spec path calls this.
fn warn_unless_aa(fg: Color, dim: Color, bg: Color) {
    if let Some(fault) = selection_pairing_fault(fg, dim, bg) {
        warn!("{fault}");
    }
}

/// Derive a foreground for `on` that starts at `from` and is guaranteed legible.
///
/// The look this exists for is a knockout: a selected row's icon and label take
/// the page's own background colour, so the selection wash reads as a solid
/// block with the content punched out of it — dark in dark mode, light in
/// light. That is the *starting point*, not the answer. `from` is returned
/// untouched whenever it already clears AA, so the intended look survives
/// wherever it is legible and is bent only where it is not.
///
/// Bending walks `from` toward the mode's extreme — black in dark, white in
/// light — desaturating as it goes, so the far end is a true black or white
/// rather than a chroma clipped to some arbitrary luminance on the way out of
/// gamut. Contrast is measured on each candidate after that clipping, so the
/// answer is what will actually be on the pixel, not what the OKLCH triple
/// claims. If the whole walk fails — a dark-mode wash too dark for black to sit
/// on — it runs the other way instead. One of the two extremes always clears
/// AA: the worst possible background sits at relative luminance 0.179, where
/// black and white both measure 4.58.
/// Fine enough that the answer never overshoots the intent by much; coarse
/// enough that building a palette stays trivial.
const CONTRAST_WALK_STEPS: u32 = 100;

/// One rung of [`contrast_checked`]'s ladder, shared with the test that proves
/// the walk stops at the *first* rung that clears — a test which must generate
/// candidates the same way without borrowing the search that picks among them.
///
/// Opaque by construction: a foreground token whose contrast has been checked
/// must be the colour that lands, not one that composites into something else.
fn walk_candidate(from: bevy::color::Oklcha, toward: f32, step: u32) -> Color {
    let t = step as f32 / CONTRAST_WALK_STEPS as f32;
    Color::from(bevy::color::Oklcha::new(
        from.lightness + (toward - from.lightness) * t,
        from.chroma * (1.0 - t),
        from.hue,
        1.0,
    ))
}

fn contrast_checked(from: Color, on: Color, mode: Mode) -> Color {
    const STEPS: u32 = CONTRAST_WALK_STEPS;
    let from = bevy::color::Oklcha::from(from);
    let walk = |toward: f32| -> Option<Color> {
        (0..=STEPS).find_map(|step| {
            let candidate = walk_candidate(from, toward, step);
            (contrast_ratio(candidate, on) >= AA_CONTRAST).then_some(candidate)
        })
    };
    let (preferred, fallback) = match mode {
        Mode::Dark => (0.0, 1.0),
        Mode::Light => (1.0, 0.0),
    };
    walk(preferred)
        .or_else(|| walk(fallback))
        .unwrap_or_else(|| {
            // Unreachable — each walk ends at pure black or pure white. Answer with
            // the better of the two rather than panic a theme build over a float
            // that came out a hair short.
            if relative_luminance(on) > 0.179 {
                Color::BLACK
            } else {
                Color::WHITE
            }
        })
}

/// Move an accent-coloured selection seed away from its panel until the bar
/// reaches the requested separation, retaining the seed's chroma and hue.
///
/// Built-in palettes are expected to reach `target`. For an arbitrary input
/// where even the mode extreme misses it, this is deliberately total: it emits
/// a warning and returns that extreme as a best effort, which does not imply
/// the requested separation was achieved.
fn separated_from(seed: Oklch, panel: Color, mode: Mode, target: f32) -> Color {
    let extreme = match mode {
        Mode::Dark => 100.0,
        Mode::Light => 0.0,
    };
    (0..=CONTRAST_WALK_STEPS)
        .find_map(|step| {
            let t = step as f32 / CONTRAST_WALK_STEPS as f32;
            let candidate = ok(seed.l + (extreme - seed.l) * t, seed.c, seed.h).color();
            (contrast_ratio(candidate, panel) >= target).then_some(candidate)
        })
        .unwrap_or_else(|| {
            warn!(
                "selection separation target {target}:1 is unreachable; \
                 using the {mode:?} mode extreme as a best-effort fallback"
            );
            ok(extreme, seed.c, seed.h).color()
        })
}

fn segment_candidate(from: bevy::color::Oklcha, to: bevy::color::Oklcha, t: f32) -> Color {
    Color::from(bevy::color::Oklcha::new(
        from.lightness + (to.lightness - from.lightness) * t,
        from.chroma + (to.chroma - from.chroma) * t,
        // Hold the knockout's hue rather than interpolating an angle linearly.
        // The built-in pairs share a hue, but arbitrary overrides may straddle
        // 0/360; holding `from` also matches the measured dim-lightness probe.
        from.hue,
        from.alpha + (to.alpha - from.alpha) * t,
    ))
}

/// Dim `text` as far toward `background` as AA permits.
///
/// This preserves AA only when the starting `text` already clears it. If no
/// rung clears — including the unchanged starting colour — the input is
/// returned unchanged rather than inventing a different illegible colour.
fn dimmed_on(text: Color, background: Color) -> Color {
    let from = bevy::color::Oklcha::from(text);
    let to = bevy::color::Oklcha::from(background);
    (0..=CONTRAST_WALK_STEPS)
        .rev()
        .find_map(|step| {
            let t = step as f32 / CONTRAST_WALK_STEPS as f32;
            let candidate = segment_candidate(from, to, t);
            (contrast_ratio(candidate, background) >= AA_CONTRAST).then_some(candidate)
        })
        .unwrap_or(text)
}

/// Make a muted foreground legible against every surface it is painted on.
///
/// If neither endpoint walk can clear every background, returns the rung with
/// the highest minimum ratio, never one worse than `from`. That best effort is
/// total for arbitrary public colours but does not itself guarantee AA.
fn legible_away(from: Color, backgrounds: &[Color], mode: Mode) -> Color {
    let from_ok = bevy::color::Oklcha::from(from);
    let minimum = |candidate: Color| {
        backgrounds
            .iter()
            .map(|background| contrast_ratio(candidate, *background))
            .fold(f32::INFINITY, f32::min)
    };
    let candidate = |toward: f32, step: u32| {
        let t = step as f32 / CONTRAST_WALK_STEPS as f32;
        Color::from(bevy::color::Oklcha::new(
            from_ok.lightness + (toward - from_ok.lightness) * t,
            from_ok.chroma,
            from_ok.hue,
            1.0,
        ))
    };
    let clears = |colour: Color| {
        backgrounds
            .iter()
            .all(|background| contrast_ratio(colour, *background) >= AA_CONTRAST)
    };
    // This is intentionally the opposite mode mapping to `contrast_checked`:
    // these backgrounds are the dark/light panel side, not the bright/dark bar.
    let (preferred, fallback) = match mode {
        Mode::Dark => (1.0, 0.0),
        Mode::Light => (0.0, 1.0),
    };
    for toward in [preferred, fallback] {
        if let Some(colour) = (0..=CONTRAST_WALK_STEPS)
            .find_map(|step| clears(candidate(toward, step)).then(|| candidate(toward, step)))
        {
            return colour;
        }
    }

    let mut best = from;
    let mut best_ratio = minimum(from);
    for toward in [preferred, fallback] {
        for step in 0..=CONTRAST_WALK_STEPS {
            let colour = candidate(toward, step);
            let ratio = minimum(colour);
            if ratio.is_finite() && (!best_ratio.is_finite() || ratio > best_ratio) {
                best = colour;
                best_ratio = ratio;
            }
        }
    }
    best
}

/// The six cosmix colour schemes — shared verbatim with the web design system.
/// Between schemes only the hue changes (plus the accent's per-scheme L/C:
/// reds punchy, stone/mono desaturated). [`Scheme::Mono`] is greyscale
/// (chroma 0) with coloured status meters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Ocean,
    Crimson,
    Stone,
    Forest,
    Sunset,
    Mono,
}

/// Light or dark. The console defaults to [`Scheme::Ocean`] + [`Mode::Light`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Light,
    Dark,
}

impl Scheme {
    pub const ALL: [Scheme; 6] = [
        Scheme::Ocean,
        Scheme::Crimson,
        Scheme::Stone,
        Scheme::Forest,
        Scheme::Sunset,
        Scheme::Mono,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Scheme::Ocean => "ocean",
            Scheme::Crimson => "crimson",
            Scheme::Stone => "stone",
            Scheme::Forest => "forest",
            Scheme::Sunset => "sunset",
            Scheme::Mono => "mono",
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        Scheme::ALL.into_iter().find(|sc| sc.name() == s)
    }
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "light" => Some(Mode::Light),
            "dark" => Some(Mode::Dark),
            _ => None,
        }
    }
}

/// The web semantic roles the console maps onto (transcribed verbatim from
/// `site.css`).
struct Roles {
    bg1: Oklch,
    bg2: Oklch,
    bg3: Oklch,
    fg: Oklch,
    fg_muted: Oklch,
    accent: Oklch,
    accent_hover: Oklch,
}

/// The web design system's exact per-(scheme, mode) palette, transcribed from
/// `/opt/cosmix/vhosts/shared/assets/site.css`. Meters are scheme-invariant
/// status colours ([`status`]).
fn web_roles(scheme: Scheme, mode: Mode) -> Roles {
    use Mode::{Dark, Light};
    use Scheme::{Crimson, Forest, Mono, Ocean, Stone, Sunset};
    match (scheme, mode) {
        (Ocean, Dark) => Roles {
            bg1: ok(12., 0.015, 220.),
            bg2: ok(16., 0.02, 220.),
            bg3: ok(22., 0.025, 220.),
            fg: ok(95., 0.02, 220.),
            fg_muted: ok(61., 0.04, 220.),
            accent: ok(75., 0.12, 220.),
            accent_hover: ok(85., 0.1, 220.),
        },
        (Ocean, Light) => Roles {
            bg1: ok(98., 0.008, 220.),
            bg2: ok(96., 0.012, 220.),
            bg3: ok(92., 0.018, 220.),
            fg: ok(25., 0.06, 220.),
            fg_muted: ok(50., 0.06, 220.),
            accent: ok(50., 0.12, 220.),
            accent_hover: ok(45., 0.14, 220.),
        },
        (Crimson, Dark) => Roles {
            bg1: ok(10., 0.015, 25.),
            bg2: ok(14., 0.02, 25.),
            bg3: ok(20., 0.025, 25.),
            fg: ok(95., 0.02, 25.),
            fg_muted: ok(61., 0.03, 25.),
            accent: ok(63., 0.23, 25.),
            accent_hover: ok(70., 0.25, 25.),
        },
        (Crimson, Light) => Roles {
            bg1: ok(98., 0.008, 25.),
            bg2: ok(96., 0.012, 25.),
            bg3: ok(92., 0.018, 25.),
            fg: ok(25., 0.04, 25.),
            fg_muted: ok(50., 0.04, 25.),
            accent: ok(47., 0.2, 25.),
            accent_hover: ok(42., 0.22, 25.),
        },
        (Stone, Dark) => Roles {
            bg1: ok(12., 0.01, 60.),
            bg2: ok(16., 0.012, 60.),
            bg3: ok(22., 0.015, 60.),
            fg: ok(95., 0.01, 60.),
            fg_muted: ok(61., 0.015, 60.),
            accent: ok(80., 0.03, 60.),
            accent_hover: ok(90., 0.02, 60.),
        },
        (Stone, Light) => Roles {
            bg1: ok(98., 0.005, 60.),
            bg2: ok(96., 0.008, 60.),
            bg3: ok(92., 0.012, 60.),
            fg: ok(25., 0.02, 60.),
            fg_muted: ok(50., 0.015, 60.),
            accent: ok(45., 0.05, 60.),
            accent_hover: ok(35., 0.06, 60.),
        },
        (Forest, Dark) => Roles {
            bg1: ok(12., 0.015, 150.),
            bg2: ok(16., 0.02, 150.),
            bg3: ok(22., 0.025, 150.),
            fg: ok(95., 0.02, 150.),
            fg_muted: ok(61., 0.04, 150.),
            accent: ok(70., 0.12, 150.),
            accent_hover: ok(80., 0.1, 150.),
        },
        (Forest, Light) => Roles {
            bg1: ok(98., 0.008, 150.),
            bg2: ok(96., 0.012, 150.),
            bg3: ok(92., 0.018, 150.),
            fg: ok(25., 0.06, 150.),
            fg_muted: ok(50., 0.06, 150.),
            accent: ok(49., 0.12, 150.),
            accent_hover: ok(44., 0.14, 150.),
        },
        (Sunset, Dark) => Roles {
            bg1: ok(12., 0.02, 45.),
            bg2: ok(16., 0.025, 45.),
            bg3: ok(22., 0.03, 45.),
            fg: ok(95., 0.025, 45.),
            fg_muted: ok(61., 0.05, 45.),
            accent: ok(72., 0.14, 45.),
            accent_hover: ok(82., 0.12, 45.),
        },
        (Sunset, Light) => Roles {
            bg1: ok(98., 0.01, 45.),
            bg2: ok(96., 0.015, 45.),
            bg3: ok(92., 0.02, 45.),
            fg: ok(30., 0.08, 45.),
            fg_muted: ok(50., 0.08, 45.),
            accent: ok(52., 0.16, 45.),
            accent_hover: ok(46., 0.18, 45.),
        },
        (Mono, Dark) => Roles {
            bg1: ok(10., 0., 0.),
            bg2: ok(15., 0., 0.),
            bg3: ok(22., 0., 0.),
            fg: ok(95., 0., 0.),
            fg_muted: ok(61., 0., 0.),
            accent: ok(85., 0., 0.),
            accent_hover: ok(92., 0., 0.),
        },
        (Mono, Light) => Roles {
            bg1: ok(98., 0., 0.),
            bg2: ok(96., 0., 0.),
            bg3: ok(92., 0., 0.),
            fg: ok(20., 0., 0.),
            fg_muted: ok(50., 0., 0.),
            accent: ok(25., 0., 0.),
            accent_hover: ok(35., 0., 0.),
        },
    }
}

/// Meter/status colours — scheme-invariant, mode-dependent (web
/// `--success`/`--warning`/`--danger`). Returns `(green, amber, red)`.
fn status(mode: Mode) -> (Oklch, Oklch, Oklch) {
    match mode {
        Mode::Dark => (ok(70., 0.15, 145.), ok(80., 0.15, 85.), ok(70., 0.18, 25.)),
        Mode::Light => (ok(45., 0.15, 145.), ok(70., 0.15, 85.), ok(52., 0.2, 25.)),
    }
}

/// The full resolved theme as data: the shared cosmix scheme + mode, the
/// console colour tokens they resolve to, and layout metrics. The built-in
/// ([`ThemeSpec::builtin`]) is Ocean/Light — the same palette the web
/// `site.css` renders, mapped onto the console. With the `theme` feature,
/// [`resolve_theme`] overlays a strict-data `.mix` file (a scheme/mode change
/// or per-token hex) on top. [`apply_theme`] paints it onto a Feathers
/// `UiTheme`. See `_decisions/2026-07-22-cosmix-visual-identity-own-palette`.
#[derive(Clone, Debug)]
pub struct ThemeSpec {
    pub scheme: Scheme,
    pub mode: Mode,
    pub colors: ThemeColors,
    pub metrics: CtkThemeMetrics,
    pub typography: TypographySpec,
    typography_family_provenance: TypographyProvenance,
    typography_body_px_provenance: TypographyProvenance,
}

/// The CTK colour-token values (the wire names are in [`tokens`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeColors {
    pub surface: Color,
    pub panel: Color,
    pub master_panel: Color,
    pub track: Color,
    pub control: Color,
    pub control_active: Color,
    pub thumb: Color,
    pub meter_green: Color,
    pub meter_amber: Color,
    pub meter_red: Color,
    pub text: Color,
    pub text_dim: Color,
    pub border: Color,
    pub row_hover: Color,
    pub row_selected: Color,
    pub row_selected_text: Color,
    pub row_selected_text_dim: Color,
    pub scrim: Color,
    pub danger_surface: Color,
}

impl ThemeSpec {
    /// The compiled-in default: the shared cosmix **Ocean** scheme, **light** —
    /// the palette the web `site.css` renders, mapped onto the console's
    /// tokens. The always-present fallback when no theme file is found.
    /// Light matches the fleet-default look (mac-light chrome — Mark,
    /// 2026-08-08); `cosmix-deco`'s default triple keys off the same call.
    pub fn builtin() -> Self {
        Self::from_scheme(Scheme::Ocean, Mode::Light)
    }

    /// Report a committed selection pairing that does not honour the
    /// guarantees [`tokens::ROW_SELECTED_TEXT`] and
    /// [`tokens::ROW_SELECTED_TEXT_DIM`] make.
    ///
    /// This deliberately sits on the complete spec rather than inside any
    /// field-by-field mutation. File overlays apply transactionally to a clone,
    /// and direct runtime specs arrive complete, so only a state that can
    /// actually be painted is reported.
    fn check_selection_contrast(&self) {
        warn_unless_aa(
            self.colors.row_selected_text,
            self.colors.row_selected_text_dim,
            self.colors.row_selected,
        );
    }

    /// Build the console palette from a shared cosmix scheme + mode. The web
    /// semantic roles map 1:1 onto most console tokens; the three console-only
    /// tokens (the `track` well, the resting `control` body, the `thumb`) are
    /// derived from them (the web has no fader/knob). Meters are the
    /// scheme-invariant status colours.
    pub fn from_scheme(scheme: Scheme, mode: Mode) -> Self {
        let r = web_roles(scheme, mode);
        let (green, amber, red) = status(mode);
        // The well sits a touch below the surface; the resting control is a
        // subtle accent-tinted body lifted off the panel. The well takes the
        // PANELS' chroma (bg3.c), not the surface's lower chroma — otherwise at
        // light-mode's high lightness it reads as a near-neutral (perceptually
        // warm/pink) strip against the blue panels instead of the same
        // blue-grey family as the master strip.
        let track = ok((r.bg1.l - 4.0).max(4.0), r.bg3.c, r.bg1.h);
        let control = ok(r.bg3.l + 3.0, (r.accent.c * 0.35).min(0.08), r.bg1.h);
        let hover_l = match mode {
            Mode::Dark => r.bg3.l + 7.0,
            Mode::Light => r.bg3.l - 7.0,
        };
        let danger_l = (r.bg3.l + red.l) * 0.5;
        let panel = r.bg2.color();
        let row_hover = ok(hover_l, (r.accent.c * 0.30).min(0.06), r.accent.h).color();
        let row_selected = separated_from(
            ok(r.accent.l, (r.accent.c * 0.72).min(0.16), r.accent.h),
            panel,
            mode,
            SELECTION_SEPARATION,
        );
        // The 7:1 bar separation means the panel itself clears AA at rung zero,
        // so `contrast_checked` returns the exact pure-knockout colour without
        // needing a special case.
        let row_selected_text = contrast_checked(panel, row_selected, mode);
        let colors = ThemeColors {
            surface: r.bg1.color(),
            panel,
            master_panel: r.bg3.color(),
            track: track.color(),
            control: control.color(),
            control_active: r.accent.color(),
            thumb: r.accent_hover.color(),
            meter_green: green.color(),
            meter_amber: amber.color(),
            meter_red: red.color(),
            text: r.fg.color(),
            text_dim: legible_away(r.fg_muted.color(), &[panel, row_hover], mode),
            border: r.bg3.color(),
            row_hover,
            row_selected,
            row_selected_text,
            row_selected_text_dim: dimmed_on(row_selected_text, row_selected),
            scrim: Color::srgba(0.0, 0.0, 0.0, if mode == Mode::Dark { 0.62 } else { 0.36 }),
            danger_surface: ok(danger_l, (red.c * 0.45).min(0.10), red.h).color(),
        };
        Self {
            scheme,
            mode,
            colors,
            metrics: CtkThemeMetrics::default(),
            typography: TypographySpec::default(),
            typography_family_provenance: TypographyProvenance::BuiltIn,
            typography_body_px_provenance: TypographyProvenance::BuiltIn,
        }
    }
}

impl Default for ThemeSpec {
    fn default() -> Self {
        Self::builtin()
    }
}

/// Paint a [`ThemeSpec`]'s palette onto a Feathers theme. Legacy non-button
/// metrics remain relaunch-only; `CtkButton` colour and geometry come from the
/// live compiled design table instead.
pub(crate) fn install_theme_spec(theme: &mut UiTheme, spec: &ThemeSpec) {
    let c = &spec.colors;
    theme.set_color("ctk.surface", c.surface);
    theme.set_color("ctk.panel", c.panel);
    theme.set_color("ctk.master.panel", c.master_panel);
    theme.set_color("ctk.track", c.track);
    theme.set_color("ctk.control", c.control);
    theme.set_color("ctk.control.active", c.control_active);
    theme.set_color("ctk.thumb", c.thumb);
    theme.set_color("ctk.meter.green", c.meter_green);
    theme.set_color("ctk.meter.amber", c.meter_amber);
    theme.set_color("ctk.meter.red", c.meter_red);
    theme.set_color("ctk.text", c.text);
    theme.set_color("ctk.text.dim", c.text_dim);
    theme.set_color("ctk.border", c.border);
    theme.set_color("ctk.row.hover", c.row_hover);
    theme.set_color("ctk.row.selected", c.row_selected);
    theme.set_color("ctk.row.selected.text", c.row_selected_text);
    theme.set_color("ctk.row.selected.text.dim", c.row_selected_text_dim);
    theme.set_color("ctk.scrim", c.scrim);
    theme.set_color("ctk.danger.surface", c.danger_surface);
}

/// Convenience lookup for dynamic repaint systems.
pub fn ctk_color(theme: &UiTheme, token: &ThemeToken) -> Color {
    theme.color(token)
}

/// Live theme identity and invalidation revision.
///
/// Runtime application updates colours, typography and the compiled button
/// table. Legacy non-button [`CtkThemeMetrics`] remain relaunch-only.
#[derive(Resource, Clone, Debug)]
pub struct ThemeState {
    /// Currently applied shared colour scheme.
    pub scheme: Scheme,
    /// Currently applied light/dark mode.
    pub mode: Mode,
    /// Monotonic restyle/cache invalidation revision.
    pub revision: u64,
    colors: ThemeColors,
    typography: TypographySpec,
    typography_family_provenance: TypographyProvenance,
    typography_body_px_provenance: TypographyProvenance,
}

impl Default for ThemeState {
    fn default() -> Self {
        let spec = ThemeSpec::builtin();
        Self {
            scheme: spec.scheme,
            mode: spec.mode,
            revision: 0,
            colors: spec.colors,
            typography: spec.typography,
            typography_family_provenance: spec.typography_family_provenance,
            typography_body_px_provenance: spec.typography_body_px_provenance,
        }
    }
}

impl ThemeState {
    fn matches(&self, spec: &ThemeSpec) -> bool {
        self.revision != 0
            && self.scheme == spec.scheme
            && self.mode == spec.mode
            && self.colors == spec.colors
            && self.typography == spec.typography
            && self.typography_family_provenance == spec.typography_family_provenance
            && self.typography_body_px_provenance == spec.typography_body_px_provenance
    }
}

/// Request a live colour and typography theme application.
///
/// Metrics carried by the spec are deliberately ignored at runtime.
#[derive(Message, Clone, Debug)]
pub struct ApplyTheme(pub ThemeSpec);

/// Apply one spec immediately and advance [`ThemeState`] when its colours,
/// typography or identity changed. This function never mutates
/// [`CtkThemeMetrics`].
///
/// This is CTK's only public runtime mutation helper. Bevy still permits
/// callers to write [`UiTheme`] directly through `ResMut<UiTheme>`; doing so
/// bypasses [`ThemeState`] and therefore does not invalidate revision-driven
/// presentation.
pub fn apply_theme(theme: &mut UiTheme, state: &mut ThemeState, spec: &ThemeSpec) -> bool {
    install_theme_spec(theme, spec);
    let changed = state.revision == 0
        || state.scheme != spec.scheme
        || state.mode != spec.mode
        || state.colors != spec.colors
        || state.typography != spec.typography
        || state.typography_family_provenance != spec.typography_family_provenance
        || state.typography_body_px_provenance != spec.typography_body_px_provenance;
    if changed {
        state.scheme = spec.scheme;
        state.mode = spec.mode;
        state.colors = spec.colors.clone();
        state.typography = spec.typography.clone();
        state.typography_family_provenance = spec.typography_family_provenance;
        state.typography_body_px_provenance = spec.typography_body_px_provenance;
        state.revision = state.revision.saturating_add(1);
        spec.check_selection_contrast();
    }
    changed
}

/// Bring any configured base size into the supported range. `f32::clamp`
/// propagates NaN, so a non-finite size falls back to the default rather than
/// poisoning every size derived from it.
fn clamp_body_px(body_px: f32) -> f32 {
    if body_px.is_finite() {
        body_px.clamp(MIN_BODY_PX, MAX_BODY_PX)
    } else {
        DEFAULT_BODY_PX
    }
}

fn scale_authored_font_size(authored_size: FontSize, body_px: f32) -> FontSize {
    authored_size * (body_px / AUTHORED_BODY_PX)
}

fn configure_typography(
    state: &ThemeState,
    typography: &mut CtkTypography,
    font_cx: &mut FontCx,
) -> bool {
    // An unresolved family is retried on every pass, not once per theme
    // revision: the font collection is built from the system at `FontCx`
    // construction, but families can still be registered into it afterwards,
    // and caching the miss against the revision would strand the mapping until
    // an unrelated theme change happened to come along. A retry that changes
    // nothing returns `false` below, so this costs one collection lookup.
    if typography.observed_theme_revision == Some(state.revision)
        && typography.fallback == TypographyFallback::Requested
    {
        // Settled — but re-assert ownership of the generic mapping rather than
        // assuming it survives. The collection can be rebuilt underneath us
        // (dropping the last strong handle to a font asset does exactly that),
        // and a mapping lost that way would leave managed text rendering
        // through some other fallback while this resource still claimed the
        // requested family was in force. A failure falls through to a full
        // re-resolution, which downgrades the state honestly.
        match typography.effective_family.as_deref() {
            Some(family) if font_cx.set_sans_serif_family(family).is_err() => {}
            _ => return false,
        }
    }

    let previous = (
        typography.effective_family.clone(),
        typography.requested_family.clone(),
        typography.body_px,
        typography.fallback,
        typography.family_provenance,
        typography.body_px_provenance,
    );
    let requested_family = state.typography.family.trim();
    typography.requested_family = requested_family.to_string();
    let requested_body_px = clamp_body_px(state.typography.body_px);

    let resolved = font_cx
        .collection
        .family_by_name(requested_family)
        .is_some()
        && font_cx.set_sans_serif_family(requested_family).is_ok();

    // The configured size is honoured whichever way the family resolves. It has
    // its own cascade provenance, so letting a missing family also revert the
    // size would make the same theme file mean different things depending on
    // whether the family happened to resolve earlier in the process's life.
    typography.body_px = requested_body_px;
    typography.body_px_provenance = state.typography_body_px_provenance;

    if resolved {
        typography.effective_family = Some(requested_family.to_string());
        typography.fallback = TypographyFallback::Requested;
        typography.family_provenance = state.typography_family_provenance;
        typography.last_warning = None;
    } else {
        // Log once per family, but keep `last_warning` populated for any run
        // that ends unresolved — including one that follows a resolved run and
        // so would otherwise report no warning at all.
        let first_sighting = typography
            .warned_families
            .insert(requested_family.to_string());
        if first_sighting || typography.last_warning.is_none() {
            let message = format!(
                "CTK typography family {requested_family:?} is unavailable; \
                 keeping the safe fallback"
            );
            if first_sighting {
                warn!("{message}");
            }
            typography.last_warning = Some(message);
        }
        // A last known-good family is only *good* while the collection still
        // maps to it. Re-assert it here too: if that fails, the mapping is
        // genuinely gone, and reporting `LastKnownGood` would have this
        // resource name a family nothing on screen is rendered in — and would
        // keep the stamping loop claiming the generic sans is ours.
        let last_good_live = match typography.effective_family.as_deref() {
            Some(family) => font_cx.set_sans_serif_family(family).is_ok(),
            None => false,
        };
        if last_good_live {
            typography.fallback = TypographyFallback::LastKnownGood;
        } else {
            typography.effective_family = None;
            typography.fallback = TypographyFallback::Embedded;
            typography.family_provenance = TypographyProvenance::EmbeddedFallback;
        }
    }
    typography.observed_theme_revision = Some(state.revision);

    let current = (
        typography.effective_family.clone(),
        typography.requested_family.clone(),
        typography.body_px,
        typography.fallback,
        typography.family_provenance,
        typography.body_px_provenance,
    );
    let changed = current != previous;
    if changed {
        typography.revision = typography.revision.saturating_add(1);
    }
    changed
}

fn apply_ctk_typography(
    mut commands: Commands,
    state: Res<ThemeState>,
    mut typography: ResMut<CtkTypography>,
    mut font_cx: Option<ResMut<FontCx>>,
    mut text_fonts: Query<TypographyTextQueryData, TypographyTextQueryFilter>,
) {
    let mut typography_changed = false;
    if let Some(font_cx) = font_cx.as_deref_mut() {
        // Deliberately bypassed: an unresolved family is re-examined on every
        // pass, and touching `ResMut` would dirty the resource's change tick
        // each frame even when nothing about the mapping moved.
        let typography = typography.bypass_change_detection();
        if configure_typography(&state, typography, font_cx) {
            typography_changed = true;
        }
    }
    if typography_changed {
        typography.set_changed();
    }
    let mapping_available = typography.effective_family.is_some();
    // `CtkTypography` is a public resource, so a caller can write `body_px`
    // directly and skip the theme cascade's bounds entirely. Normalise it here,
    // in the resource itself rather than only in the derived sizes: an agent
    // reading this resource must see the size that is actually in force.
    let body_px = clamp_body_px(typography.body_px);
    if typography.body_px != body_px {
        typography.body_px = body_px;
    }

    for (entity, mut font, managed, opted_out) in &mut text_fonts {
        if opted_out {
            continue;
        }
        if !is_manageable_size(font.font_size) {
            continue;
        }

        // Reconcile rather than stamp once: an entity whose size no longer
        // matches what CTK last saw has been reassigned by somebody else, and
        // that write is the new authored intent. A changed *source* is not a
        // size write — re-adopting the already-scaled size there would multiply
        // it on every pass.
        let mut record = match managed.as_deref() {
            Some(managed) => *managed,
            None => ManagedTypography {
                authored_size: font.font_size,
                last_seen_size: font.font_size,
            },
        };
        if record.last_seen_size != font.font_size {
            record.authored_size = font.font_size;
        }

        // The size is a theme value in its own right and applies whether or not
        // the family resolved — otherwise a single typo in the family name
        // would silently abandon the base size the operator also asked for, and
        // leave already-managed text stranded at whatever size was in force
        // when the mapping was last good.
        let effective = scale_authored_font_size(record.authored_size, body_px);
        // Only the *source* is gated on actually owning the generic mapping:
        // stamping `SansSerif` without it would hand text to whatever
        // fontconfig picks rather than Bevy's embedded font. A source already
        // stamped is never reverted, though — the embedded fallback is an
        // ASCII-only subset, so unwinding to it on a lost mapping would trade
        // a working face for tofu.
        let source_drift = mapping_available && !matches!(font.font, FontSource::SansSerif);
        if font.font_size != effective || source_drift {
            if mapping_available {
                font.font = FontSource::SansSerif;
            }
            font.font_size = effective;
            font.set_changed();
        }
        record.last_seen_size = effective;

        match managed {
            Some(mut managed) => {
                managed.set_if_neq(record);
            }
            None => {
                commands.entity(entity).insert(record);
            }
        }
    }
}

/// Ask CTK to persist a theme selection without blocking the Bevy main thread.
///
/// [`CtkThemePlugin`] sends the complete locked read-modify-write to its
/// dedicated I/O worker. A [`ThemeWriteCompleted`] message is published later
/// with the result; applications should consume failures there for user-facing
/// status or toast presentation.
#[cfg(feature = "theme")]
#[derive(Message, Clone, Debug, Eq, PartialEq)]
pub struct ThemeWriteRequest {
    /// Theme file to update.
    pub path: std::path::PathBuf,
    /// Scheme selection to persist.
    pub scheme: Scheme,
    /// Light/dark mode selection to persist.
    pub mode: Mode,
}

#[cfg(feature = "theme")]
impl ThemeWriteRequest {
    /// Build a request for an explicit theme file.
    pub fn new(path: impl Into<std::path::PathBuf>, scheme: Scheme, mode: Mode) -> Self {
        Self {
            path: path.into(),
            scheme,
            mode,
        }
    }

    /// Build a request for the desktop-wide shared theme file.
    pub fn shared(scheme: Scheme, mode: Mode) -> Self {
        Self::new(shared_theme_path(), scheme, mode)
    }
}

/// Result of one asynchronous [`ThemeWriteRequest`].
#[cfg(feature = "theme")]
#[derive(Message, Clone, Debug, Eq, PartialEq)]
pub struct ThemeWriteCompleted {
    /// Theme file targeted by the request.
    pub path: std::path::PathBuf,
    /// Scheme selection requested.
    pub scheme: Scheme,
    /// Light/dark mode selection requested.
    pub mode: Mode,
    /// Successful persistence or a stable human-readable error.
    pub result: Result<(), String>,
}

/// Runtime colour-theme application and live `theme.conf.mix` reload support.
///
/// With the `theme` feature, CTK watches the shared and optional per-app theme
/// directories. Focus gain and Bus invalidations feed the same reload path as
/// missed-event backstops. Legacy non-button metrics remain launch-time only.
pub struct CtkThemePlugin {
    app_config_dir: Option<std::path::PathBuf>,
    #[cfg(feature = "theme")]
    shared_path: std::path::PathBuf,
}

impl CtkThemePlugin {
    /// Build the runtime plugin for an optional per-app theme directory.
    pub fn new(app_config_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            app_config_dir,
            #[cfg(feature = "theme")]
            shared_path: plugin_shared_theme_path(),
        }
    }
}

#[cfg(all(feature = "theme", not(test)))]
fn plugin_shared_theme_path() -> std::path::PathBuf {
    shared_theme_path()
}

// Unit tests must never inherit the operator's real desktop theme. Tests that
// exercise shared-file loading inject their own explicit temp path.
#[cfg(all(feature = "theme", test))]
fn plugin_shared_theme_path() -> std::path::PathBuf {
    std::env::temp_dir()
        .join(format!("ctk-test-no-shared-theme-{}", std::process::id()))
        .join(THEME_FILE)
}

impl Default for CtkThemePlugin {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Plugin for CtkThemePlugin {
    fn build(&self, app: &mut App) {
        crate::design::init_design_resources(app);
        app.init_resource::<UiTheme>()
            .init_resource::<CtkThemeMetrics>()
            .init_resource::<CtkTypography>()
            .init_resource::<ThemeState>()
            .add_message::<ApplyTheme>()
            .add_systems(Update, apply_theme_requests)
            .add_systems(
                PostUpdate,
                apply_ctk_typography
                    .in_set(UiSystems::Propagate)
                    .after(PropagateSet::<TextFont>::default())
                    .before(detect_text_needs_rerender),
            )
            .add_systems(
                PostUpdate,
                // After the typography pass so a Startup-spawned button label
                // adopts the resolved family before its first render; before
                // rerender detection so that adoption lands the same frame.
                crate::button::reconcile_button_label_fonts
                    .in_set(UiSystems::Propagate)
                    .after(apply_ctk_typography)
                    .before(detect_text_needs_rerender),
            )
            .add_systems(
                PostUpdate,
                // UiSystems::Propagate intentionally runs before Bevy's Layout
                // set, so elision observes the previous frame's ComputedNode.
                // Moving this after Layout would make the Text write require a
                // second layout pass in the same frame; one clipped/stale frame
                // on spawn or resize is the cheaper and bounded trade-off.
                crate::text_elide::update_middle_elided_text
                    .in_set(UiSystems::Propagate)
                    .after(apply_ctk_typography)
                    .before(detect_text_needs_rerender)
                    .run_if(
                        bevy::ecs::schedule::common_conditions::resource_exists::<TextPipeline>,
                    ),
            );
        #[cfg(feature = "icons")]
        app.add_systems(
            Update,
            (
                crate::icons::retint_added_icons,
                crate::icons::retint_icons_on_theme_change.after(apply_theme_requests),
            ),
        );
        #[cfg(feature = "theme")]
        {
            let reload = ThemeReloadSignal::default();
            let shared_path = lexical_absolute_theme_path(&self.shared_path);
            let app_config_dir = self
                .app_config_dir
                .as_deref()
                .map(lexical_absolute_theme_path);
            let app_theme_path = app_config_dir
                .as_deref()
                .map(|directory| directory.join(THEME_FILE));
            let startup_file_exists = shared_path.exists()
                || app_theme_path
                    .as_deref()
                    .is_some_and(std::path::Path::exists);
            if startup_file_exists {
                reload.request_reload();
            }
            app.insert_resource(ThemeRuntimeConfig {
                shared_path,
                app_config_dir,
            })
            .init_resource::<ThemeLayerLastGood>()
            .insert_resource(StartupDiskDesignSourceLog {
                pending: startup_file_exists,
            })
            .insert_resource(reload)
            .insert_resource(ThemeWriteWorker::start())
            .add_message::<AppExit>()
            .add_message::<WindowFocused>()
            .add_message::<ThemeWriteRequest>()
            .add_message::<ThemeWriteCompleted>()
            .add_systems(PreStartup, start_theme_file_watcher)
            .add_systems(
                Update,
                (
                    reload_theme_on_focus.before(reload_theme_files),
                    reload_theme_files.before(apply_theme_requests),
                    service_theme_writes,
                ),
            )
            .add_systems(bevy::app::Last, shutdown_theme_writer);
        }
        #[cfg(all(feature = "theme", feature = "bus"))]
        app.add_systems(
            Update,
            crate::theme_sync::receive_theme_changed.before(reload_theme_files),
        )
        .add_systems(
            bevy::app::Last,
            crate::theme_sync::publish_shared_theme_changes,
        );
        #[cfg(not(feature = "theme"))]
        let _ = &self.app_config_dir;
    }
}

pub(crate) fn apply_theme_requests(
    mut requests: MessageReader<ApplyTheme>,
    mut theme: ResMut<UiTheme>,
    mut state: ResMut<ThemeState>,
) {
    for request in requests.read() {
        if state.matches(&request.0) {
            continue;
        }
        apply_theme(&mut theme, &mut state, &request.0);
    }
}

#[cfg(feature = "theme")]
#[derive(Resource)]
pub(crate) struct ThemeRuntimeConfig {
    pub(crate) shared_path: std::path::PathBuf,
    pub(crate) app_config_dir: Option<std::path::PathBuf>,
}

#[cfg(feature = "theme")]
fn lexical_absolute_theme_path(path: &std::path::Path) -> std::path::PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(feature = "theme")]
fn resolved_theme_path(path: &std::path::Path) -> std::path::PathBuf {
    let absolute = lexical_absolute_theme_path(path);
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return canonical;
    }
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            return canonical_parent.join(name);
        }
    }
    absolute
}

#[cfg(feature = "theme")]
#[derive(Default, Resource)]
pub(crate) struct ThemeLayerLastGood {
    shared: CachedThemeLayer,
    app: CachedThemeLayer,
}

#[cfg(feature = "theme")]
#[derive(Default)]
struct CachedThemeLayer {
    path: Option<std::path::PathBuf>,
    palette: Option<ThemeFile>,
    design: Option<CachedDesignLayer>,
}

#[cfg(feature = "theme")]
enum CachedDesignLayer {
    Absent,
    Present(Vec<u8>),
}

#[cfg(feature = "theme")]
#[derive(Resource)]
pub(crate) struct StartupDiskDesignSourceLog {
    pending: bool,
}

#[cfg(feature = "theme")]
#[derive(Default)]
struct ThemeReloadState {
    pending: std::sync::atomic::AtomicBool,
    pending_reload_count: std::sync::atomic::AtomicU64,
    logged_failures: std::sync::Mutex<std::collections::VecDeque<u64>>,
}

#[cfg(feature = "theme")]
const LOGGED_THEME_FAILURE_CAPACITY: usize = 8;

/// Coalesced ingress shared by the directory watcher, focus backstop and Bus.
#[cfg(feature = "theme")]
#[derive(Clone, Default, Resource)]
pub(crate) struct ThemeReloadSignal(std::sync::Arc<ThemeReloadState>);

#[cfg(feature = "theme")]
impl ThemeReloadSignal {
    /// Returns true only for the first request before the pending reload runs.
    pub(crate) fn request_reload(&self) -> bool {
        if self
            .0
            .pending
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return false;
        }
        self.0
            .pending_reload_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    fn take_pending(&self) -> bool {
        self.0
            .pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    fn log_failure_once(&self, fingerprint: u64, message: &str) {
        let mut logged = self
            .0
            .logged_failures
            .lock()
            .expect("CTK theme failure cache poisoned");
        if logged.contains(&fingerprint) {
            return;
        }
        if logged.len() == LOGGED_THEME_FAILURE_CAPACITY {
            logged.pop_front();
        }
        logged.push_back(fingerprint);
        warn!("CTK theme layer rejected: {message}");
    }

    #[cfg(test)]
    fn pending_reload_count(&self) -> u64 {
        self.0
            .pending_reload_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    fn logged_failure_count(&self) -> usize {
        self.0
            .logged_failures
            .lock()
            .expect("CTK theme failure cache poisoned")
            .len()
    }
}

#[cfg(feature = "theme")]
#[derive(Resource)]
pub(crate) struct ThemeFileWatcher {
    /// Stable configured identities. These remain lexical so replacing a
    /// symlink in the configured path changes what the next reload reads.
    targets: std::sync::Arc<Vec<std::path::PathBuf>>,
    event_paths: std::sync::Arc<std::sync::RwLock<ThemeWatchPaths>>,
    state: std::sync::Mutex<ThemeWatcherState>,
    watch_generation: std::sync::atomic::AtomicU64,
    watch_invalidation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(feature = "theme")]
struct ThemeWatcherState {
    watcher: notify::RecommendedWatcher,
    watched_parents: std::collections::HashMap<std::path::PathBuf, DirectoryIdentity>,
    observed_invalidation: u64,
}

#[cfg(feature = "theme")]
#[derive(Clone, Default)]
struct ThemeWatchPaths {
    targets: Vec<std::path::PathBuf>,
    parents: Vec<std::path::PathBuf>,
}

#[cfg(all(feature = "theme", unix))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(all(feature = "theme", not(unix)))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryIdentity(std::path::PathBuf);

#[cfg(feature = "theme")]
impl ThemeFileWatcher {
    fn ensure_watches(&self) {
        use notify::Watcher;

        let event_paths = theme_watch_paths(&self.targets);
        let parents: std::collections::HashSet<_> = event_paths.parents.iter().cloned().collect();
        *self
            .event_paths
            .write()
            .expect("CTK theme watcher paths poisoned") = event_paths;
        let mut state = self.state.lock().expect("CTK theme watcher poisoned");
        let invalidation = self
            .watch_invalidation
            .load(std::sync::atomic::Ordering::Acquire);
        let force_reinstall = state.observed_invalidation != invalidation;
        let mut all_watched = true;
        let obsolete: Vec<_> = state
            .watched_parents
            .keys()
            .filter(|parent| !parents.contains(*parent))
            .cloned()
            .collect();
        for parent in obsolete {
            let _ = state.watcher.unwatch(&parent);
            state.watched_parents.remove(&parent);
        }
        for parent in parents {
            if force_reinstall && state.watched_parents.contains_key(&parent) {
                let _ = state.watcher.unwatch(&parent);
                state.watched_parents.remove(&parent);
            }
            if let Err(error) = std::fs::create_dir_all(&parent) {
                warn!(
                    "CTK cannot create theme directory {} for watching: {error}",
                    parent.display()
                );
                all_watched = false;
                continue;
            }
            let Some(identity) = directory_identity(&parent) else {
                warn!(
                    "CTK cannot identify theme directory {} for watching",
                    parent.display()
                );
                all_watched = false;
                continue;
            };
            if state.watched_parents.get(&parent) == Some(&identity) {
                continue;
            }
            if state.watched_parents.contains_key(&parent) {
                let _ = state.watcher.unwatch(&parent);
                state.watched_parents.remove(&parent);
            }
            match state
                .watcher
                .watch(&parent, notify::RecursiveMode::NonRecursive)
            {
                Ok(()) => {
                    state.watched_parents.insert(parent, identity);
                    self.watch_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) => {
                    all_watched = false;
                    warn!(
                        "CTK cannot watch theme directory {}: {error}",
                        parent.display()
                    );
                }
            }
        }
        if all_watched {
            state.observed_invalidation = invalidation;
        }
    }

    #[cfg(test)]
    fn watch_generation(&self) -> u64 {
        self.watch_generation
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(feature = "theme")]
fn theme_watch_paths(targets: &[std::path::PathBuf]) -> ThemeWatchPaths {
    fn push_unique(paths: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }

    let mut result = ThemeWatchPaths::default();
    for configured in targets {
        let configured = lexical_absolute_theme_path(configured);
        let resolved = resolved_theme_path(&configured);
        push_unique(&mut result.targets, configured.clone());
        push_unique(&mut result.targets, resolved.clone());
        if let Some(parent) = resolved.parent() {
            push_unique(&mut result.parents, parent.to_path_buf());
        }

        // A watch installed through a symlink follows the destination inode
        // and cannot see the directory entry itself being replaced. Watch the
        // containing lexical directory and accept the symlink path as a
        // target so an atomic rename-to causes the next reload to re-resolve.
        let mut prefix = std::path::PathBuf::new();
        for component in configured.components() {
            prefix.push(component.as_os_str());
            let is_symlink = std::fs::symlink_metadata(&prefix)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                push_unique(&mut result.targets, prefix.clone());
                if let Some(parent) = prefix.parent() {
                    push_unique(&mut result.parents, parent.to_path_buf());
                }
            }
        }
    }
    result
}

#[cfg(all(feature = "theme", unix))]
fn directory_identity(path: &std::path::Path) -> Option<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(all(feature = "theme", not(unix)))]
fn directory_identity(path: &std::path::Path) -> Option<DirectoryIdentity> {
    std::fs::canonicalize(path).ok().map(DirectoryIdentity)
}

#[cfg(feature = "theme")]
const THEME_WRITE_QUEUE_CAPACITY: usize = 8;

#[cfg(feature = "theme")]
#[derive(Resource)]
struct ThemeWriteWorker {
    requests: Option<std::sync::mpsc::SyncSender<ThemeWriteRequest>>,
    completions: std::sync::Mutex<std::sync::mpsc::Receiver<ThemeWriteCompleted>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "theme")]
impl ThemeWriteWorker {
    fn start() -> Self {
        let (request_tx, request_rx) =
            std::sync::mpsc::sync_channel::<ThemeWriteRequest>(THEME_WRITE_QUEUE_CAPACITY);
        let (completion_tx, completion_rx) = std::sync::mpsc::channel::<ThemeWriteCompleted>();
        let handle = std::thread::Builder::new()
            .name("ctk-theme-writer".to_string())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let result =
                        file::write_theme_selection(&request.path, request.scheme, request.mode);
                    let completed = ThemeWriteCompleted {
                        path: request.path,
                        scheme: request.scheme,
                        mode: request.mode,
                        result,
                    };
                    if completion_tx.send(completed).is_err() {
                        break;
                    }
                }
            })
            .expect("CTK theme writer thread must start");
        Self {
            requests: Some(request_tx),
            completions: std::sync::Mutex::new(completion_rx),
            handle: Some(handle),
        }
    }

    fn shutdown(&mut self) {
        self.requests.take();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                eprintln!("ctk theme: writer thread panicked during shutdown");
            }
        }
    }
}

#[cfg(feature = "theme")]
impl Drop for ThemeWriteWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(feature = "theme")]
fn service_theme_writes(
    mut requests: MessageReader<ThemeWriteRequest>,
    worker: bevy::ecs::system::Res<ThemeWriteWorker>,
    mut completed: MessageWriter<ThemeWriteCompleted>,
) {
    for request in requests.read() {
        let result = worker
            .requests
            .as_ref()
            .ok_or_else(|| std::sync::mpsc::TrySendError::Disconnected(request.clone()))
            .and_then(|requests| requests.try_send(request.clone()));
        if let Err(error) = result {
            let (request, detail) = match error {
                std::sync::mpsc::TrySendError::Full(request) => {
                    (request, "theme write queue is full")
                }
                std::sync::mpsc::TrySendError::Disconnected(request) => {
                    (request, "theme write worker is unavailable")
                }
            };
            completed.write(ThemeWriteCompleted {
                path: request.path,
                scheme: request.scheme,
                mode: request.mode,
                result: Err(detail.to_string()),
            });
        }
    }

    publish_theme_write_completions(&worker, &mut completed);
}

#[cfg(feature = "theme")]
fn shutdown_theme_writer(
    mut exits: MessageReader<AppExit>,
    mut worker: bevy::ecs::system::ResMut<ThemeWriteWorker>,
    mut completed: MessageWriter<ThemeWriteCompleted>,
) {
    if exits.read().next().is_none() {
        return;
    }
    worker.shutdown();
    publish_theme_write_completions(&worker, &mut completed);
}

#[cfg(feature = "theme")]
fn publish_theme_write_completions(
    worker: &ThemeWriteWorker,
    completed: &mut MessageWriter<ThemeWriteCompleted>,
) {
    let results = worker
        .completions
        .lock()
        .expect("CTK theme completion queue poisoned");
    for result in results.try_iter() {
        completed.write(result);
    }
}

#[cfg(feature = "theme")]
fn reload_theme_on_focus(
    mut focused: MessageReader<WindowFocused>,
    reload: bevy::ecs::system::Res<ThemeReloadSignal>,
) {
    let mut gained = false;
    for event in focused.read() {
        gained |= event.focused;
    }
    if gained {
        reload.request_reload();
    }
}

#[cfg(feature = "theme")]
pub(crate) fn reload_theme_files(
    config: Res<ThemeRuntimeConfig>,
    reload: Res<ThemeReloadSignal>,
    watcher: Option<Res<ThemeFileWatcher>>,
    mut startup_source_log: Option<ResMut<StartupDiskDesignSourceLog>>,
    mut last_good: ResMut<ThemeLayerLastGood>,
    mut design: ResMut<crate::design::CtkDesignStatus>,
    mut requests: MessageWriter<ApplyTheme>,
) {
    if !reload.take_pending() {
        return;
    }

    if let Some(watcher) = watcher {
        watcher.ensure_watches();
    }

    let app_path = config
        .app_config_dir
        .as_deref()
        .map(|directory| directory.join(THEME_FILE));
    let shared = read_theme_layer(&config.shared_path);
    let app = app_path.as_deref().map(read_theme_layer);
    requests.write(ApplyTheme(resolve_snapshot_theme(
        &shared,
        app.as_ref(),
        &reload,
        &mut last_good,
    )));

    let disk_design = resolve_cached_design(&last_good);
    let log_startup_selection = startup_source_log
        .as_mut()
        .is_some_and(|log| std::mem::take(&mut log.pending));
    if log_startup_selection {
        if let DiskDesignSource::File { path, layer, .. } = &disk_design {
            info!(
                "CTK design source selected: {} (layer={})",
                path.display(),
                layer.name()
            );
        }
    }
    match disk_design {
        DiskDesignSource::Embedded => design.use_embedded_source(),
        DiskDesignSource::File { path, bytes, .. } => {
            design.replace_source_bytes(path.to_string_lossy(), bytes);
        }
    }
}

#[cfg(feature = "theme")]
enum DiskDesignSource {
    Embedded,
    File {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
        layer: ThemeLayer,
    },
}

#[cfg(feature = "theme")]
#[derive(Clone, Copy)]
enum ThemeLayer {
    App,
    Shared,
}

#[cfg(feature = "theme")]
impl ThemeLayer {
    const fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Shared => "shared",
        }
    }
}

#[cfg(feature = "theme")]
const MAX_THEME_FILE_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(feature = "theme")]
enum ThemeLayerSnapshot {
    Missing,
    Rejected { fingerprint: u64, message: String },
    Loaded(Box<LoadedThemeLayer>),
}

#[cfg(feature = "theme")]
struct LoadedThemeLayer {
    path: std::path::PathBuf,
    bytes: Vec<u8>,
    has_design: bool,
    theme: Result<ThemeFile, String>,
}

#[cfg(feature = "theme")]
fn read_theme_layer(path: &std::path::Path) -> ThemeLayerSnapshot {
    use std::io::Read;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ThemeLayerSnapshot::Missing;
        }
        Err(error) => {
            return ThemeLayerSnapshot::Rejected {
                fingerprint: theme_layer_fingerprint(path, &[], "open-error"),
                message: format!("theme {} could not be opened: {error}", path.display()),
            };
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_THEME_FILE_BYTES + 1).read_to_end(&mut bytes) {
        return ThemeLayerSnapshot::Rejected {
            fingerprint: theme_layer_fingerprint(path, &bytes, "read-error"),
            message: format!("theme {} could not be read: {error}", path.display()),
        };
    }
    if bytes.is_empty() {
        return ThemeLayerSnapshot::Rejected {
            fingerprint: theme_layer_fingerprint(path, &bytes, "empty"),
            message: format!("theme {} is empty", path.display()),
        };
    }
    if bytes.len() as u64 > MAX_THEME_FILE_BYTES {
        return ThemeLayerSnapshot::Rejected {
            fingerprint: theme_layer_fingerprint(path, &bytes, "too-large"),
            message: format!(
                "theme {} exceeds the {} MiB limit",
                path.display(),
                MAX_THEME_FILE_BYTES / (1024 * 1024)
            ),
        };
    }
    let source = match std::str::from_utf8(&bytes) {
        Ok(source) => source,
        Err(error) => {
            return ThemeLayerSnapshot::Rejected {
                fingerprint: theme_layer_fingerprint(path, &bytes, "invalid-utf8"),
                message: format!("theme {} is not UTF-8: {error}", path.display()),
            };
        }
    };
    let value = match cosmix_config::parse_mix_data(source) {
        Ok(value) => value,
        Err(error) => {
            return ThemeLayerSnapshot::Rejected {
                fingerprint: theme_layer_fingerprint(path, &bytes, "parse-error"),
                message: format!("theme {} could not be parsed: {error}", path.display()),
            };
        }
    };
    let cosmix_mix::value::Value::Map(map) = &value else {
        return ThemeLayerSnapshot::Rejected {
            fingerprint: theme_layer_fingerprint(path, &bytes, "not-a-map"),
            message: format!("theme {} must contain a top-level map", path.display()),
        };
    };
    let has_design = map.contains_key("design");
    let theme = cosmix_mix::from_value(&value)
        .map_err(|error| format!("theme {}: {error}", path.display()));
    ThemeLayerSnapshot::Loaded(Box::new(LoadedThemeLayer {
        path: path.to_path_buf(),
        bytes,
        has_design,
        theme,
    }))
}

#[cfg(feature = "theme")]
fn resolve_snapshot_theme(
    shared: &ThemeLayerSnapshot,
    app: Option<&ThemeLayerSnapshot>,
    reload: &ThemeReloadSignal,
    last_good: &mut ThemeLayerLastGood,
) -> ThemeSpec {
    let mut spec = ThemeSpec::builtin();
    apply_snapshot_layer(
        &mut spec,
        shared,
        &mut last_good.shared,
        TypographyProvenance::SharedTheme,
        reload,
    );
    if let Some(app) = app {
        apply_snapshot_layer(
            &mut spec,
            app,
            &mut last_good.app,
            TypographyProvenance::AppTheme,
            reload,
        );
    } else {
        last_good.app = CachedThemeLayer::default();
    }
    spec.check_selection_contrast();
    spec
}

#[cfg(feature = "theme")]
fn apply_snapshot_layer(
    spec: &mut ThemeSpec,
    snapshot: &ThemeLayerSnapshot,
    last_good: &mut CachedThemeLayer,
    provenance: TypographyProvenance,
    reload: &ThemeReloadSignal,
) {
    match snapshot {
        ThemeLayerSnapshot::Missing => {
            *last_good = CachedThemeLayer::default();
        }
        ThemeLayerSnapshot::Rejected {
            fingerprint,
            message,
        } => {
            reload.log_failure_once(*fingerprint, message);
            apply_cached_palette(spec, last_good, provenance, reload);
        }
        ThemeLayerSnapshot::Loaded(layer) => match &layer.theme {
            Ok(theme) => {
                let mut candidate = spec.clone();
                match candidate.overlay_with_provenance(theme, provenance) {
                    Ok(()) => {
                        *spec = candidate;
                        last_good.path = Some(layer.path.clone());
                        last_good.palette = Some(theme.clone());
                        last_good.design = Some(if layer.has_design {
                            CachedDesignLayer::Present(layer.bytes.clone())
                        } else {
                            CachedDesignLayer::Absent
                        });
                    }
                    Err(error) => {
                        let message = format!(
                            "theme {}: {error} (last-good palette retained)",
                            layer.path.display()
                        );
                        reload.log_failure_once(
                            theme_layer_fingerprint(&layer.path, &layer.bytes, "palette-overlay"),
                            &message,
                        );
                        apply_cached_palette(spec, last_good, provenance, reload);
                    }
                }
            }
            Err(message) => {
                reload.log_failure_once(
                    theme_layer_fingerprint(&layer.path, &layer.bytes, "palette-deserialize"),
                    message,
                );
                apply_cached_palette(spec, last_good, provenance, reload);
            }
        },
    }
}

#[cfg(feature = "theme")]
fn apply_cached_palette(
    spec: &mut ThemeSpec,
    last_good: &CachedThemeLayer,
    provenance: TypographyProvenance,
    reload: &ThemeReloadSignal,
) {
    let (Some(path), Some(palette)) = (&last_good.path, &last_good.palette) else {
        return;
    };
    if let Err(error) = spec.overlay_with_provenance(palette, provenance) {
        let message = format!(
            "theme {}: cached palette could not be reapplied: {error}",
            path.display()
        );
        reload.log_failure_once(
            theme_layer_fingerprint(path, &[], "cached-palette-overlay"),
            &message,
        );
    }
}

/// Select the highest-precedence last accepted layer that contains a `design`
/// key.
/// Its complete original byte snapshot remains the compiler authority across
/// a rejected mid-save read, including when the value itself is nil, partial or
/// otherwise invalid. A committed map without the key or a removal clears that
/// layer's cached design authority.
#[cfg(feature = "theme")]
fn resolve_cached_design(last_good: &ThemeLayerLastGood) -> DiskDesignSource {
    for (cached, precedence) in [
        (&last_good.app, ThemeLayer::App),
        (&last_good.shared, ThemeLayer::Shared),
    ] {
        if let (Some(path), Some(CachedDesignLayer::Present(bytes))) =
            (&cached.path, &cached.design)
        {
            return DiskDesignSource::File {
                path: path.clone(),
                bytes: bytes.clone(),
                layer: precedence,
            };
        }
    }
    DiskDesignSource::Embedded
}

#[cfg(feature = "theme")]
fn theme_layer_fingerprint(path: &std::path::Path, bytes: &[u8], category: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    bytes.hash(&mut hasher);
    category.hash(&mut hasher);
    hasher.finish()
}

#[cfg(feature = "theme")]
fn start_theme_file_watcher(
    mut commands: Commands,
    config: Res<ThemeRuntimeConfig>,
    reload: Res<ThemeReloadSignal>,
    event_loop_proxy: Option<Res<bevy::winit::EventLoopProxyWrapper>>,
) {
    let mut paths = vec![config.shared_path.clone()];
    if let Some(app_config_dir) = &config.app_config_dir {
        paths.push(app_config_dir.join(THEME_FILE));
    }
    let wake: std::sync::Arc<dyn Fn() + Send + Sync> = if let Some(proxy) = event_loop_proxy {
        let proxy = (**proxy).clone();
        std::sync::Arc::new(move || {
            let _ = proxy.send_event(bevy::winit::WinitUserEvent::WakeUp);
        })
    } else {
        std::sync::Arc::new(|| {})
    };
    match theme_file_watcher(paths, (*reload).clone(), wake) {
        Ok(watcher) => {
            commands.insert_resource(watcher);
        }
        Err(error) => warn!("CTK theme directory watcher unavailable: {error}"),
    }
}

#[cfg(feature = "theme")]
pub(crate) fn theme_file_watcher(
    paths: Vec<std::path::PathBuf>,
    reload: ThemeReloadSignal,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> Result<ThemeFileWatcher, String> {
    let targets = std::sync::Arc::new(
        paths
            .iter()
            .map(|path| lexical_absolute_theme_path(path))
            .collect::<Vec<_>>(),
    );
    let event_paths = std::sync::Arc::new(std::sync::RwLock::new(theme_watch_paths(&targets)));
    let callback_event_paths = std::sync::Arc::clone(&event_paths);
    let watch_invalidation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let callback_invalidation = std::sync::Arc::clone(&watch_invalidation);
    let watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) => {
                let event_paths = callback_event_paths
                    .read()
                    .expect("CTK theme watcher paths poisoned");
                if theme_event_rechecks_watches(&event, &event_paths) {
                    callback_invalidation.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
                if theme_event_requests_reload(&event, &event_paths) && reload.request_reload() {
                    wake();
                }
            }
            Err(error) => {
                warn!("CTK theme directory watcher error: {error}");
                callback_invalidation.fetch_add(1, std::sync::atomic::Ordering::Release);
                if reload.request_reload() {
                    wake();
                }
            }
        })
        .map_err(|error| error.to_string())?;
    let result = ThemeFileWatcher {
        targets,
        event_paths,
        state: std::sync::Mutex::new(ThemeWatcherState {
            watcher,
            watched_parents: std::collections::HashMap::new(),
            observed_invalidation: 0,
        }),
        watch_generation: std::sync::atomic::AtomicU64::new(0),
        watch_invalidation,
    };
    result.ensure_watches();
    Ok(result)
}

#[cfg(feature = "theme")]
fn theme_event_requests_reload(event: &notify::Event, paths: &ThemeWatchPaths) -> bool {
    use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
    use notify::EventKind;

    if theme_event_rechecks_watches(event, paths) {
        return true;
    }
    let committed_change = matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both))
            | EventKind::Remove(_)
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    );
    committed_change
        && event
            .paths
            .iter()
            .any(|changed| paths.targets.iter().any(|target| changed == target))
}

#[cfg(feature = "theme")]
fn theme_event_rechecks_watches(event: &notify::Event, paths: &ThemeWatchPaths) -> bool {
    event.need_rescan()
        || event.paths.is_empty()
        || event
            .paths
            .iter()
            .any(|changed| paths.parents.iter().any(|parent| changed == parent))
}

// ── Data-driven theme files (feature `theme`) ──────────────────────────────
//
// A strict-data `.mix` theme file overlays the built-in: `built-in ← shared ←
// per-app`. Every field is optional, so a file may override just a few tokens
// or metrics. Colours are `#rrggbb` hex; metrics are pixels. Parsing goes
// through cosmix-lib-config (the mandated substrate strict-data path — never
// TOML/JSON). The live reload path snapshots each layer once, retains the
// last-good value across malformed or mid-save reads. Palette validation and
// `design` authority commit together, so a rejected layer cannot split them.
#[cfg(feature = "theme")]
mod file {
    use super::{
        contrast_checked, dimmed_on, legible_away, Color, CtkThemeMetrics, Mode, Scheme, ThemeSpec,
        TypographyProvenance, MAX_BODY_PX, MIN_BODY_PX,
    };
    use std::fs::{File, OpenOptions};
    use std::path::Path;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    const THEME_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
    const THEME_LOCK_RETRY: Duration = Duration::from_millis(10);

    /// The on-disk strict-data theme (`theme.conf.mix`). All fields optional.
    /// Unknown fields are ignored (forward-compat: a newer file degrades
    /// gracefully on an older ctk). `scheme`/`mode` pick a shared cosmix
    /// palette (the common case, matching the web `setScheme`/`setTheme`); the
    /// per-token hex fields are escape-hatch overrides applied on top.
    #[derive(Clone, Debug, Default, serde::Deserialize)]
    #[serde(default)]
    pub struct ThemeFile {
        pub scheme: Option<String>,
        pub mode: Option<String>,
        /// Compatibility sink only. The live loader detects key presence in
        /// the parsed strict-data map before this typed deserialisation, then
        /// `cosmix-design` validates the complete original byte snapshot.
        pub design: Option<serde::de::IgnoredAny>,
        pub typography: Option<TypographyFile>,
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
        pub control_gap: Option<f32>,
        pub corner_radius: Option<f32>,
        pub fader_width: Option<f32>,
        pub fader_height: Option<f32>,
        pub knob_size: Option<f32>,
        pub meter_width: Option<f32>,
    }

    /// Optional typography fields nested under `typography` in a theme file.
    #[derive(Clone, Debug, Default, serde::Deserialize)]
    #[serde(default)]
    pub struct TypographyFile {
        pub family: Option<String>,
        pub body_px: Option<f32>,
    }

    /// Parse a `#rrggbb` (or `#rgb`) hex colour into an sRGB [`Color`].
    fn hex(s: &str) -> Result<Color, String> {
        bevy::color::Srgba::hex(s)
            .map(Color::from)
            .map_err(|e| format!("bad colour {s:?}: {e}"))
    }

    impl ThemeSpec {
        /// Overlay a file's present fields onto this spec; absent fields keep
        /// the base value (partial override is the point of the cascade). A
        /// `scheme`/`mode` change rebuilds the whole palette from the shared
        /// cosmix model FIRST, then per-token hex fields override on top. A
        /// malformed value returns `Err` with the offending field — the caller
        /// decides whether to skip the file.
        pub fn overlay(&mut self, file: &ThemeFile) -> Result<(), String> {
            self.overlay_with_provenance(file, TypographyProvenance::DirectApply)?;
            self.check_selection_contrast();
            Ok(())
        }

        pub(super) fn overlay_with_provenance(
            &mut self,
            file: &ThemeFile,
            provenance: TypographyProvenance,
        ) -> Result<(), String> {
            let mut candidate = self.clone();
            candidate.overlay_validated(file, provenance)?;
            *self = candidate;
            Ok(())
        }

        fn overlay_validated(
            &mut self,
            file: &ThemeFile,
            provenance: TypographyProvenance,
        ) -> Result<(), String> {
            // A scheme/mode change reselects the shared palette before any
            // per-token hex override is applied.
            let mut reselect = false;
            if let Some(s) = &file.scheme {
                self.scheme =
                    Scheme::from_name(s).ok_or_else(|| format!("unknown scheme {s:?}"))?;
                reselect = true;
            }
            if let Some(m) = &file.mode {
                self.mode = Mode::from_name(m).ok_or_else(|| format!("unknown mode {m:?}"))?;
                reselect = true;
            }
            if reselect {
                self.colors = ThemeSpec::from_scheme(self.scheme, self.mode).colors;
            }
            let c = &mut self.colors;
            if let Some(v) = &file.surface {
                c.surface = hex(v)?;
            }
            if let Some(v) = &file.panel {
                c.panel = hex(v)?;
            }
            if let Some(v) = &file.master_panel {
                c.master_panel = hex(v)?;
            }
            if let Some(v) = &file.track {
                c.track = hex(v)?;
            }
            if let Some(v) = &file.control {
                c.control = hex(v)?;
            }
            if let Some(v) = &file.control_active {
                c.control_active = hex(v)?;
            }
            if let Some(v) = &file.thumb {
                c.thumb = hex(v)?;
            }
            if let Some(v) = &file.meter_green {
                c.meter_green = hex(v)?;
            }
            if let Some(v) = &file.meter_amber {
                c.meter_amber = hex(v)?;
            }
            if let Some(v) = &file.meter_red {
                c.meter_red = hex(v)?;
            }
            if let Some(v) = &file.text {
                c.text = hex(v)?;
            }
            if let Some(v) = &file.border {
                c.border = hex(v)?;
            }
            if let Some(v) = &file.row_hover {
                c.row_hover = hex(v)?;
            }
            if let Some(v) = &file.text_dim {
                c.text_dim = hex(v)?;
            } else if file.panel.is_some() || file.row_hover.is_some() {
                // `text_dim` is derived against both surfaces. If either moves
                // without an explicit replacement, the carried value is stale
                // and must be re-derived against the final parsed pair.
                c.text_dim = legible_away(c.text_dim, &[c.panel, c.row_hover], self.mode);
            }
            if let Some(v) = &file.row_selected {
                c.row_selected = hex(v)?;
            }
            if let Some(v) = &file.row_selected_text {
                c.row_selected_text = hex(v)?;
            } else if file.row_selected.is_some() || file.panel.is_some() {
                // Either half of the derivation moved, so the value carried in
                // is stale — re-derive it. A file that mentions neither leaves
                // whatever is already there.
                //
                // This is layer-local, not provenance-tracking: the cascade
                // cannot tell an explicit foreground inherited from an earlier
                // layer from a derived one, so a later layer that moves only
                // `panel` re-derives over an earlier layer's explicit value. A
                // theme that wants its foreground to survive that must restate
                // it in the same layer that moves either half.
                c.row_selected_text = contrast_checked(c.panel, c.row_selected, self.mode);
            }
            if let Some(v) = &file.row_selected_text_dim {
                c.row_selected_text_dim = hex(v)?;
            } else if file.row_selected.is_some()
                || file.panel.is_some()
                || file.row_selected_text.is_some()
            {
                c.row_selected_text_dim = dimmed_on(c.row_selected_text, c.row_selected);
            }
            if let Some(v) = &file.scrim {
                c.scrim = hex(v)?;
            }
            if let Some(v) = &file.danger_surface {
                c.danger_surface = hex(v)?;
            }
            if let Some(typography) = &file.typography {
                if let Some(family) = &typography.family {
                    let family = family.trim();
                    if family.is_empty() {
                        return Err("typography.family must not be empty".to_string());
                    }
                    self.typography.family = family.to_string();
                    self.typography_family_provenance = provenance;
                }
                if let Some(body_px) = typography.body_px {
                    if !body_px.is_finite() || !(MIN_BODY_PX..=MAX_BODY_PX).contains(&body_px) {
                        return Err(format!(
                            "typography.body_px must be a finite number between \
                             {MIN_BODY_PX} and {MAX_BODY_PX}"
                        ));
                    }
                    self.typography.body_px = body_px;
                    self.typography_body_px_provenance = provenance;
                }
            }
            let m: &mut CtkThemeMetrics = &mut self.metrics;
            if let Some(v) = file.control_gap {
                m.control_gap = v;
            }
            if let Some(v) = file.corner_radius {
                m.corner_radius = v;
            }
            if let Some(v) = file.fader_width {
                m.fader_width = v;
            }
            if let Some(v) = file.fader_height {
                m.fader_height = v;
            }
            if let Some(v) = file.knob_size {
                m.knob_size = v;
            }
            if let Some(v) = file.meter_width {
                m.meter_width = v;
            }
            Ok(())
        }
    }

    /// Parse one theme file (strict-data `.mix`). `Ok(None)` if it does not
    /// exist; `Err` on a present-but-unparseable file.
    pub fn load_theme_file(path: &Path) -> Result<Option<ThemeFile>, String> {
        if !path.exists() {
            return Ok(None);
        }
        cosmix_config::load_conf_mix_path::<ThemeFile>(path)
            .map(Some)
            .map_err(|e| format!("theme {}: {e}", path.display()))
    }

    /// Resolve the effective theme: `built-in ← shared ← per-app override`.
    /// Each path optional; a missing file is skipped and a malformed one is
    /// logged and skipped — a broken theme never bricks the app.
    pub fn resolve_theme(shared: Option<&Path>, app: Option<&Path>) -> ThemeSpec {
        let mut spec = ThemeSpec::builtin();
        for (path, provenance) in [
            (shared, TypographyProvenance::SharedTheme),
            (app, TypographyProvenance::AppTheme),
        ] {
            let Some(path) = path else {
                continue;
            };
            match load_theme_file(path) {
                Ok(Some(f)) => {
                    if let Err(e) = spec.overlay_with_provenance(&f, provenance) {
                        eprintln!("ctk theme: {e} (skipped)");
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("ctk theme: {e} (skipped)"),
            }
        }
        spec.check_selection_contrast();
        spec
    }

    /// Resolve `built-in selection ← shared overrides ← per-app overrides`.
    ///
    /// Unlike [`resolve_theme`], the caller's `scheme` and `mode` are
    /// authoritative: any selection fields in either file are ignored while
    /// token and metric overrides still cascade normally. This is the runtime
    /// companion to a theme-selection write, preventing a live apply from
    /// briefly dropping custom colours before the persisted selection is
    /// reloaded.
    pub fn resolve_theme_with_selection(
        shared: Option<&Path>,
        app: Option<&Path>,
        scheme: Scheme,
        mode: Mode,
    ) -> ThemeSpec {
        let mut spec = ThemeSpec::from_scheme(scheme, mode);
        for (path, provenance) in [
            (shared, TypographyProvenance::SharedTheme),
            (app, TypographyProvenance::AppTheme),
        ] {
            let Some(path) = path else {
                continue;
            };
            match load_theme_file(path) {
                Ok(Some(mut file)) => {
                    file.scheme = None;
                    file.mode = None;
                    if let Err(error) = spec.overlay_with_provenance(&file, provenance) {
                        eprintln!("ctk theme: {error} (skipped)");
                    }
                }
                Ok(None) => {}
                Err(error) => eprintln!("ctk theme: {error} (skipped)"),
            }
        }
        spec.check_selection_contrast();
        spec
    }

    /// The standard cosmix theme file name (shared dir + per-app dir alike).
    pub const THEME_FILE: &str = "theme.conf.mix";

    /// The shared cosmix theme path — the `theme.conf.mix` below
    /// `cosmix_config::store::config_dir()` (`COSMIX_ETC`, a located checkout's
    /// `$COSMIX/etc`, or the platform XDG/FHS default).
    pub fn shared_theme_path() -> std::path::PathBuf {
        cosmix_config::store::config_dir().join(THEME_FILE)
    }

    /// Resolve the effective theme for an app: `built-in ← shared cosmix theme
    /// ← per-app override`. `app_config_dir` is the app's own config dir (its
    /// `theme.conf.mix` overrides the shared one); pass `None` to skip the
    /// per-app layer. Missing/malformed files are skipped.
    pub fn resolve_app_theme(app_config_dir: Option<&Path>) -> ThemeSpec {
        let shared = shared_theme_path();
        let app = app_config_dir.map(|d| d.join(THEME_FILE));
        resolve_theme(Some(&shared), app.as_deref())
    }

    /// Resolve an app theme using a new authoritative selection while
    /// preserving the shared and per-app token/metric override cascade.
    pub fn resolve_app_theme_with_selection(
        app_config_dir: Option<&Path>,
        scheme: Scheme,
        mode: Mode,
    ) -> ThemeSpec {
        let shared = shared_theme_path();
        let app = app_config_dir.map(|directory| directory.join(THEME_FILE));
        resolve_theme_with_selection(Some(&shared), app.as_deref(), scheme, mode)
    }

    pub(super) fn theme_lock_path(path: &Path) -> Result<std::path::PathBuf, String> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} has no file name", path.display()))?;
        Ok(path.with_file_name(format!(".{name}.lock")))
    }

    /// Acquire the persistent sidecar advisory lock for a theme transaction.
    ///
    /// The sidecar is deliberately not deleted: unlinking a lock file can let
    /// a new opener lock a different inode while an older waiter still holds
    /// the original. A two-second bound prevents a crashed or hostile peer
    /// from occupying CTK's dedicated writer indefinitely.
    pub(super) fn acquire_theme_write_lock(path: &Path) -> Result<File, String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        let lock_path = theme_lock_path(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("opening theme lock {}: {error}", lock_path.display()))?;
        let deadline = Instant::now() + THEME_LOCK_TIMEOUT;
        loop {
            match lock.try_lock() {
                Ok(()) => return Ok(lock),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(THEME_LOCK_RETRY);
                }
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(THEME_LOCK_RETRY);
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(format!(
                        "timed out waiting for theme lock {}",
                        lock_path.display()
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(format!(
                        "locking theme transaction {}: {error}",
                        lock_path.display()
                    ));
                }
            }
        }
    }

    /// Transactionally change only `scheme` and `mode` in a theme file.
    ///
    /// Recognised token/metric overrides and unknown forward-compatible fields
    /// survive the strict-data read-modify-write. The complete candidate is
    /// parsed and overlaid into a clone before the atomic replacement; a
    /// malformed existing or candidate file is left untouched. Writers
    /// serialize through a persistent sidecar advisory lock, with a bounded
    /// wait, so every transaction reads the previous writer's complete result.
    pub(super) fn write_theme_selection(
        path: &Path,
        scheme: Scheme,
        mode: Mode,
    ) -> Result<(), String> {
        let _lock = acquire_theme_write_lock(path)?;
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
            Err(error) => return Err(format!("reading theme {}: {error}", path.display())),
        };
        let mut value = cosmix_config::parse_mix_data(&source)
            .map_err(|error| format!("parsing theme {}: {error}", path.display()))?;
        let cosmix_config::Value::Map(entries) = &mut value else {
            return Err(format!("theme {} must contain a map", path.display()));
        };
        let entries = Rc::make_mut(entries);
        entries.insert(
            "scheme".to_string(),
            cosmix_config::Value::String(scheme.name().to_string()),
        );
        entries.insert(
            "mode".to_string(),
            cosmix_config::Value::String(mode.name().to_string()),
        );
        let candidate = value
            .to_mix_data_string()
            .map_err(|error| format!("serialising theme {}: {error}", path.display()))?;
        let file: ThemeFile = cosmix_config::from_conf_mix_str(&candidate)
            .map_err(|error| format!("validating theme {}: {error}", path.display()))?;
        let mut validation = ThemeSpec::builtin();
        // The non-reporting overlay, deliberately. This asks only "does the file
        // parse"; it is one layer against the built-in, not the cascade anyone
        // will see. Reporting here would warn about a shared-file pairing that
        // the per-app layer repairs, on every scheme or mode change.
        validation
            .overlay_with_provenance(&file, TypographyProvenance::DirectApply)
            .map_err(|error| format!("validating theme {}: {error}", path.display()))?;
        crate::fs::write_atomic(path, candidate.as_bytes())
    }
}

#[cfg(feature = "theme")]
pub use file::{
    load_theme_file, resolve_app_theme, resolve_app_theme_with_selection, resolve_theme,
    resolve_theme_with_selection, shared_theme_path, ThemeFile, TypographyFile, THEME_FILE,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn design_context(scheme: Scheme, mode: Mode) -> cosmix_design::DesignContext {
        let scheme = match scheme {
            Scheme::Ocean => cosmix_design::Scheme::Ocean,
            Scheme::Crimson => cosmix_design::Scheme::Crimson,
            Scheme::Stone => cosmix_design::Scheme::Stone,
            Scheme::Forest => cosmix_design::Scheme::Forest,
            Scheme::Sunset => cosmix_design::Scheme::Sunset,
            Scheme::Mono => cosmix_design::Scheme::Mono,
        };
        let mode = match mode {
            Mode::Light => cosmix_design::Mode::Light,
            Mode::Dark => cosmix_design::Mode::Dark,
        };
        cosmix_design::DesignContext {
            scheme,
            mode,
            ..Default::default()
        }
    }

    fn ctk_role_anchors(scheme: Scheme, mode: Mode) -> [(&'static str, Oklch, f64); 11] {
        let roles = web_roles(scheme, mode);
        let (success, warning, danger) = status(mode);
        [
            ("palette.background.1", roles.bg1, 1.0),
            ("palette.background.2", roles.bg2, 1.0),
            ("palette.background.3", roles.bg3, 1.0),
            ("palette.foreground.default", roles.fg, 1.0),
            ("palette.foreground.muted", roles.fg_muted, 1.0),
            ("palette.accent.default", roles.accent, 1.0),
            ("palette.accent.hover", roles.accent_hover, 1.0),
            ("status.success", success, 1.0),
            ("status.warning", warning, 1.0),
            ("status.danger", danger, 1.0),
            ("transparent", ok(0.0, 0.0, 0.0), 0.0),
        ]
    }

    fn assert_anchor_matches(
        name: &str,
        scheme: Scheme,
        mode: Mode,
        ctk: cosmix_design::LinearRgba,
        compiled: cosmix_design::LinearRgba,
    ) {
        const CHANNEL_TOLERANCE: f64 = 3.0e-8;
        for (channel, ctk, compiled) in [
            ("red", ctk.red, compiled.red),
            ("green", ctk.green, compiled.green),
            ("blue", ctk.blue, compiled.blue),
            ("alpha", ctk.alpha, compiled.alpha),
        ] {
            assert!(
                (ctk - compiled).abs() <= CHANNEL_TOLERANCE,
                "web-anchor-verbatim mismatch for anchor `{name}` in context ({}, {}), \
                 channel `{channel}`: ctk={ctk:.12}, compiled={compiled:.12}",
                scheme.name(),
                mode.name(),
            );
        }
    }

    #[test]
    fn web_anchor_verbatim() {
        let document = cosmix_design::parse_design_source(
            cosmix_design::SourceIdentity::new("embedded:web-anchor-verbatim"),
            cosmix_design::EMBEDDED_DEFAULT_SOURCE,
        )
        .expect("the embedded design source parses");

        for scheme in Scheme::ALL {
            for mode in [Mode::Light, Mode::Dark] {
                let context = design_context(scheme, mode);
                let cosmix_design::DesignCompileResult::Success(compiled) =
                    cosmix_design::compile_design(&document, context.clone())
                else {
                    panic!(
                        "embedded design does not compile for ({}, {})",
                        scheme.name(),
                        mode.name()
                    );
                };

                let anchors = ctk_role_anchors(scheme, mode);
                let mut ctk_source = document.v1.clone();
                ctk_source.primitives.colors = anchors
                    .iter()
                    .map(|(name, value, alpha)| {
                        (
                            (*name).to_owned(),
                            cosmix_design::OklchSource {
                                color_space: cosmix_design::ColourSpace::Oklch,
                                l: f64::from(value.l) / 100.0,
                                c: f64::from(value.c),
                                h: f64::from(value.h),
                                alpha: *alpha,
                            },
                        )
                    })
                    .collect();
                ctk_source.primitives.colors.extend([
                    (
                        "web-anchor-verbatim.black".to_owned(),
                        cosmix_design::OklchSource {
                            color_space: cosmix_design::ColourSpace::Oklch,
                            l: 0.0,
                            c: 0.0,
                            h: 0.0,
                            alpha: 1.0,
                        },
                    ),
                    (
                        "web-anchor-verbatim.white".to_owned(),
                        cosmix_design::OklchSource {
                            color_space: cosmix_design::ColourSpace::Oklch,
                            l: 1.0,
                            c: 0.0,
                            h: 0.0,
                            alpha: 1.0,
                        },
                    ),
                ]);
                ctk_source.semantics.pairs = cosmix_design::TEXT_PAIR_NAMES
                    .into_iter()
                    .map(|name| {
                        (
                            name.to_owned(),
                            cosmix_design::PairSource::authored(
                                "web-anchor-verbatim.black",
                                "web-anchor-verbatim.white",
                                None,
                            ),
                        )
                    })
                    .collect();
                ctk_source.semantics.non_text = cosmix_design::NON_TEXT_NAMES
                    .into_iter()
                    .map(|name| {
                        (
                            name.to_owned(),
                            cosmix_design::NonTextColourSource {
                                value: "web-anchor-verbatim.white".to_owned(),
                                adjacent: vec!["base".to_owned()],
                            },
                        )
                    })
                    .collect();
                let ctk = cosmix_design::compile_colour_tokens(&ctk_source, context)
                    .expect("ctk's role anchors compile through the design colour pipeline");

                for (name, _, _) in anchors {
                    assert_anchor_matches(
                        name,
                        scheme,
                        mode,
                        ctk.value.primitives[name],
                        compiled.candidate.dictionary().colours.primitives[name],
                    );
                }
            }
        }
    }

    #[test]
    fn v0_gate_and_ctk_hex_decoders_agree() {
        use bevy::color::{ColorToPacked, Srgba};

        for value in [
            "#000",
            "#fff",
            "#1aF",
            "#0000",
            "#ffff",
            "#1aF8",
            "#000000",
            "#ffffff",
            "#12aBcF",
            "#00000000",
            "#ffffffff",
            "#12aBcF80",
            // The reader strips an optional `#`, so a hashless value is a
            // legal v0 colour and the gate has to decode it identically.
            "f3fafc",
            "0000",
            "1aF",
            "12aBcF80",
        ] {
            assert_eq!(
                cosmix_design::parse_legacy_v0_hex_colour(value).expect("v0 gate accepts colour"),
                Srgba::hex(value).expect("CTK accepts colour").to_u8_array(),
                "decoder disagreement for {value}"
            );
        }
    }

    /// Capture the warnings a call actually emits.
    ///
    /// The selection check's entire output is a `warn!`, so nothing short of
    /// reading the log proves it ran: a deleted call site, or a deleted `warn!`,
    /// is otherwise invisible to every assertion. Reading the log rather than
    /// instrumenting `check_selection_contrast` also keeps test-only code out of
    /// the production path.
    ///
    /// The subscriber is installed **globally, once**, and routes each event to
    /// a thread-local sink that only an armed `warnings_from` is holding. A
    /// thread-scoped `with_default` looks safer and is not: tracing caches
    /// callsite interest and the max level *per process*, so a callsite first
    /// reached from a thread with no subscriber is cached as uninteresting and
    /// every later thread skips it — which is exactly the intermittent empty
    /// capture this replaces. Installing globally keeps interest permanently
    /// enabled; the thread-local sink keeps two concurrent tests apart.
    #[cfg(feature = "theme")]
    fn warnings_from(call: impl FnOnce()) -> Vec<String> {
        use bevy::log::tracing;
        use bevy::log::tracing_subscriber::layer::{Context, Layer, SubscriberExt};
        use bevy::log::tracing_subscriber::registry::Registry;
        use std::cell::RefCell;
        use std::sync::Once;

        thread_local! {
            /// `Some` only while this thread is inside `warnings_from`; every
            /// other thread's warnings land in its own `None` and are dropped.
            static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
        }

        struct Sink;
        struct Message<'a>(&'a mut Vec<String>);

        impl tracing::field::Visit for Message<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                // `warn!("{fault}")` arrives as the event's `message` field.
                if field.name() == "message" {
                    self.0.push(format!("{value:?}"));
                }
            }
        }

        impl<S: tracing::Subscriber> Layer<S> for Sink {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                if *event.metadata().level() > tracing::Level::WARN {
                    return;
                }
                // `try_with`, not `with`. This layer is the process-global
                // default and outlives every thread, so it can be reached from
                // inside another thread-local's destructor — and TLS is torn
                // down in reverse initialisation order, so `SINK` may already
                // be gone by then. `with` panics in that window, and a panic
                // during TLS teardown is an unwind out of a destructor: the
                // runtime aborts the whole test binary rather than unwinding.
                // A warning logged that late has no armed sink to reach
                // anyway, so dropping it costs nothing.
                let _ = SINK.try_with(|sink| {
                    if let Some(captured) = sink.borrow_mut().as_mut() {
                        event.record(&mut Message(captured));
                    }
                });
            }
        }

        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(Registry::default().with(Sink))
                .expect("no other subscriber may own this test binary's global default");
        });

        SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
        call();
        SINK.with(|sink| sink.borrow_mut().take())
            .expect("the sink is armed immediately above")
    }

    /// A warning logged from a thread-local destructor must not kill the run.
    ///
    /// The capture layer is the process-global default and outlives every
    /// thread, so it is still reachable while a thread tears its locals down —
    /// in reverse initialisation order, which puts `SINK` first whenever some
    /// other local was initialised before the first captured warning. Reaching
    /// a destroyed local with `with` panics, and panicking out of a destructor
    /// aborts the process rather than failing one test, so this cannot be
    /// written as a `should_panic`: without `try_with` the whole binary dies
    /// and every other result is lost with it.
    #[cfg(feature = "theme")]
    #[test]
    fn a_warning_from_thread_local_teardown_does_not_abort_the_run() {
        struct WarnOnDrop;
        impl Drop for WarnOnDrop {
            fn drop(&mut self) {
                warn!("emitted from a thread-local destructor");
            }
        }
        thread_local! {
            static FIRST: WarnOnDrop = const { WarnOnDrop };
        }

        // The capture layer installs lazily on the first `warnings_from`. Without
        // this the worker's warnings reach no subscriber at all and the probe
        // proves nothing — which is exactly how it first passed against `with`.
        assert!(warnings_from(|| warn!("install the global capture layer"))
            .iter()
            .any(|line| line.contains("install the global capture layer")));

        std::thread::spawn(|| {
            // Initialised BEFORE any warning, so `SINK` — first touched by the
            // `warn!` below — is destroyed before it.
            FIRST.with(|_| {});
            warn!("arms the capture layer's local on this thread");
        })
        .join()
        .expect("the worker must exit cleanly, not abort");
    }

    #[test]
    fn builtin_is_ocean_light_from_the_shared_web_palette() {
        let spec = ThemeSpec::builtin();
        assert_eq!(spec.scheme, Scheme::Ocean);
        assert_eq!(spec.mode, Mode::Light);
        assert_eq!(spec.typography.family, "Noto Sans");
        assert_eq!(spec.typography.body_px, 13.333);
        // surface is the web Ocean-light bg-primary: oklch(98% .008 220).
        assert_eq!(spec.colors.surface, ok(98., 0.008, 220.).color());
        // control.active is the web accent: oklch(50% .12 220).
        assert_eq!(spec.colors.control_active, ok(50., 0.12, 220.).color());
        assert_eq!(spec.metrics.fader_height, 250.0);
        assert_eq!(ThemeSpec::default().colors.thumb, spec.colors.thumb);
    }

    #[test]
    fn builtin_palettes_have_pure_knockouts_and_all_derived_contrast_guarantees() {
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let c = ThemeSpec::from_scheme(scheme, mode).colors;
                let separation = contrast_ratio(c.row_selected, c.panel);
                assert!(
                    separation >= SELECTION_SEPARATION,
                    "{scheme:?}/{mode:?}: selection bar measures {separation:.2}:1 against \
                     panel, below {SELECTION_SEPARATION}:1"
                );
                // Exact panel identity is a valuable consequence of the 7:1
                // separation in these twelve built-in palettes, not a universal
                // override invariant. Arbitrary committed pairings universally
                // guarantee AA through `selection_pairing_fault`, not identity.
                assert_eq!(
                    c.row_selected_text, c.panel,
                    "{scheme:?}/{mode:?}: the selected-row foreground is not the pure knockout"
                );
                let dim = contrast_ratio(c.row_selected_text_dim, c.row_selected);
                assert!(
                    dim >= AA_CONTRAST,
                    "{scheme:?}/{mode:?}: dim selected-row text measures {dim:.2}:1, below AA"
                );
                assert_ne!(
                    c.row_selected_text_dim, c.row_selected_text,
                    "{scheme:?}/{mode:?}: selection separation left no real dim headroom"
                );
                for (name, background) in [("panel", c.panel), ("row.hover", c.row_hover)] {
                    let measured = contrast_ratio(c.text_dim, background);
                    assert!(
                        measured >= AA_CONTRAST,
                        "{scheme:?}/{mode:?}: text.dim on {name} measures \
                         {measured:.2}:1, below AA"
                    );
                }
            }
        }
    }

    /// A named foreground/background pairing, picked out of any palette.
    type Pairing = (&'static str, fn(&ThemeColors) -> (Color, Color));

    /// Measure `pairs` across all six schemes in both modes, so one run reports
    /// the whole picture rather than the first combination that trips.
    fn measure(pairs: &[Pairing]) -> Vec<(&'static str, String, f32)> {
        let mut out = Vec::new();
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let c = ThemeSpec::from_scheme(scheme, mode).colors;
                for (name, pick) in pairs {
                    let (fg, bg) = pick(&c);
                    out.push((
                        *name,
                        format!("{scheme:?}/{mode:?}"),
                        contrast_ratio(fg, bg),
                    ));
                }
            }
        }
        out
    }

    #[test]
    fn every_shipped_foreground_pairing_clears_aa() {
        // The permanent guard. A palette edit that pushes any of these under AA
        // fails here rather than in someone's eyes. Pairings CTK has not fixed
        // yet are deliberately absent and retain a measured floor below.
        let pairs: &[Pairing] = &[
            ("row.selected.text on row.selected", |c| {
                (c.row_selected_text, c.row_selected)
            }),
            ("row.selected.text.dim on row.selected", |c| {
                (c.row_selected_text_dim, c.row_selected)
            }),
            ("text.dim on row.hover", |c| (c.text_dim, c.row_hover)),
            ("text.dim on panel", |c| (c.text_dim, c.panel)),
            ("text on row.hover", |c| (c.text, c.row_hover)),
            ("text on surface", |c| (c.text, c.surface)),
            ("text on panel", |c| (c.text, c.panel)),
            ("text on master.panel", |c| (c.text, c.master_panel)),
            ("text on track", |c| (c.text, c.track)),
            ("text on control", |c| (c.text, c.control)),
            ("text on danger.surface", |c| (c.text, c.danger_surface)),
        ];
        // `!(m >= AA)` rather than `m < AA`, so a NaN ratio from a non-finite
        // channel is reported instead of silently passing both comparisons.
        #[expect(
            clippy::neg_cmp_op_on_partial_ord,
            reason = "the negation is the assertion: NaN must fail this filter, and \
                      `partial_cmp` would only restate that at more length"
        )]
        let failures: Vec<_> = measure(pairs)
            .into_iter()
            .filter(|(_, _, m)| !(*m >= AA_CONTRAST))
            .map(|(name, at, m)| format!("{at} {name}: {m:.2}:1"))
            .collect();
        assert!(
            failures.is_empty(),
            "below WCAG AA:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn foreground_pairings_keep_their_pinned_floors() {
        // The remaining control.active pairing retains its historical measured
        // floor until that separate icon treatment is changed.
        let pairs: &[Pairing] = &[
            // Folder icons drawn in the active-control colour, hovered.
            ("control.active on row.hover", |c| {
                (c.control_active, c.row_hover)
            }),
        ];
        let floor = |name: &str| match name {
            "control.active on row.hover" => 2.60,
            other => unreachable!("no pinned floor for {other}"),
        };
        let measured = measure(pairs);
        // `!(m >= floor)` for the same reason as the permanent guard: a NaN
        // ratio must read as a failure, not slip through both comparisons.
        #[expect(
            clippy::neg_cmp_op_on_partial_ord,
            reason = "the negation is the assertion: NaN must fail this filter, and \
                      `partial_cmp` would only restate that at more length"
        )]
        let worse: Vec<_> = measured
            .iter()
            .filter(|(name, _, m)| !(*m >= floor(name)))
            .map(|(name, at, m)| format!("{at} {name}: {m:.2} < {}", floor(name)))
            .collect();
        // Only a *whole* pairing counts as fixed — several of these have always
        // cleared AA on some schemes and not others.
        let fixed: Vec<_> = pairs
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| {
                measured
                    .iter()
                    .filter(|(n, _, _)| n == name)
                    .all(|(_, _, m)| *m >= AA_CONTRAST)
            })
            .collect();
        assert!(
            worse.is_empty(),
            "a known gap got worse:\n{}",
            worse.join("\n")
        );
        assert!(
            fixed.is_empty(),
            "these pairings now clear AA everywhere — delete their entries and \
             add them to `every_shipped_foreground_pairing_clears_aa`:\n{}",
            fixed.join("\n")
        );
    }

    #[test]
    fn a_translucent_override_is_reported_rather_than_measured() {
        // `#ffffff00` on black measures a perfect 21:1 and paints nothing, so
        // the ratio must not be the thing consulted. The override still stands
        // — CTK warns, it does not refuse — but `is_opaque` is what decides
        // whether the AA guarantee is claimable at all.
        let invisible = Color::srgba(1.0, 1.0, 1.0, 0.0);
        assert!(
            contrast_ratio(invisible, Color::BLACK) > 20.0,
            "the trap this guards is that alpha-blind measurement *passes*"
        );
        assert!(!is_opaque(invisible));
        assert!(!is_opaque(Color::srgba(0.0, 0.0, 0.0, 0.5)));
        assert!(is_opaque(Color::BLACK) && is_opaque(Color::WHITE));
        // Every colour the palette derives is opaque, so the guarantee holds
        // for everything CTK ships without an override.
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let c = ThemeSpec::from_scheme(scheme, mode).colors;
                assert!(
                    is_opaque(c.row_selected_text)
                        && is_opaque(c.row_selected_text_dim)
                        && is_opaque(c.row_selected),
                    "{scheme:?}/{mode:?} derives a translucent selection pairing"
                );
            }
        }
    }

    #[test]
    fn an_unmeasurable_or_failing_pairing_is_faulted_not_waved_through() {
        // The three ways the guarantee can be lost, each asserted on the value
        // the production path consults — a `warn!` needs a subscriber to see,
        // which is how a deleted check stays invisible to a test.
        let translucent =
            selection_pairing_fault(Color::srgba(1.0, 1.0, 1.0, 0.0), Color::WHITE, Color::BLACK)
                .expect("a translucent foreground measuring 21:1 must be faulted");
        assert!(translucent.contains("translucent"));

        // Reachable through the public `ThemeColors`, and the reason the check
        // is written as the negation of `>=`: NaN must fail closed.
        let nan =
            selection_pairing_fault(Color::srgb(f32::NAN, 0.0, 0.0), Color::BLACK, Color::WHITE)
                .expect("a non-finite channel must be faulted, not silently passed");
        assert!(nan.contains("non-finite"));

        let infinite_rgb = selection_pairing_fault(
            Color::srgb(f32::INFINITY, 0.0, 0.0),
            Color::BLACK,
            Color::WHITE,
        )
        .expect("an infinite RGB channel must be faulted before contrast clamps it");
        assert!(infinite_rgb.contains("non-finite"));

        let infinite_alpha = selection_pairing_fault(
            Color::srgba(1.0, 1.0, 1.0, f32::INFINITY),
            Color::WHITE,
            Color::BLACK,
        )
        .expect("an infinite alpha must be faulted before the opacity comparison");
        assert!(infinite_alpha.contains("non-finite"));

        let low = selection_pairing_fault(
            Color::srgb(0.5, 0.5, 0.5),
            Color::WHITE,
            Color::srgb(0.45, 0.45, 0.45),
        )
        .expect("a genuinely low ratio must be faulted");
        assert!(low.contains("below WCAG AA"));

        let low_dim =
            selection_pairing_fault(Color::WHITE, Color::srgb(0.2, 0.2, 0.2), Color::BLACK)
                .expect("a low dim pairing must be faulted independently");
        assert!(low_dim.contains("row_selected_text_dim"));

        // And the honest passes, so the fault is not simply always Some — the
        // extremes, and every pairing CTK actually derives.
        assert!(selection_pairing_fault(Color::WHITE, Color::WHITE, Color::BLACK).is_none());
        for scheme in Scheme::ALL {
            for mode in [Mode::Dark, Mode::Light] {
                let c = ThemeSpec::from_scheme(scheme, mode).colors;
                assert!(
                    selection_pairing_fault(
                        c.row_selected_text,
                        c.row_selected_text_dim,
                        c.row_selected,
                    )
                    .is_none(),
                    "{scheme:?}/{mode:?} derives a pairing CTK would warn about"
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "theme")]
    fn the_cascade_checks_the_theme_it_actually_applies() {
        // Every assertion here reads the warning the check actually emitted, so
        // deleting a call site — or the `warn!` itself — fails the test rather
        // than passing silently.
        let file = |s: &str| -> ThemeFile {
            cosmix_config::from_conf_mix_str(s).expect("theme file parses")
        };
        // A translucent wash is the cheapest reliable fault: it is unmeasurable
        // whatever the palette does, so this never turns on a contrast number.
        let bad = "row_selected: \"#00000080\"\n";
        let faulted = |warnings: &[String]| {
            warnings
                .iter()
                .any(|w| w.contains("translucent") && w.contains("row_selected"))
        };

        // A layer that parses and is translucent commits, and is reported.
        let mut spec = ThemeSpec::builtin();
        let committed = warnings_from(|| spec.overlay(&file(bad)).unwrap());
        assert!(
            faulted(&committed),
            "`overlay` must report the pairing it committed, got {committed:?}"
        );

        // The check runs on committed state, so a layer rejected for an
        // unrelated malformed field never reports the pairing it was going to
        // apply — and does not move the spec either.
        let mut spec = ThemeSpec::builtin();
        let before = spec.colors.row_selected_text;
        let before_dim = spec.colors.row_selected_text_dim;
        let mut err = String::new();
        let rejected = warnings_from(|| {
            err = spec
                .overlay(&file(&format!("{bad}scrim: \"not-a-colour\"\n")))
                .expect_err("a malformed scrim must reject the layer");
        });
        assert!(err.contains("not-a-colour"), "unexpected error: {err}");
        assert_eq!(
            spec.colors.row_selected_text, before,
            "a rejected layer must not have moved the selection pairing"
        );
        assert_eq!(
            spec.colors.row_selected_text_dim, before_dim,
            "a rejected layer must not have moved the dim selection pairing"
        );
        assert!(
            !faulted(&rejected),
            "a rejected layer must not be reported — the pairing it carried was \
             never applied; got {rejected:?}"
        );

        // Validating a theme write is one layer against the built-in, not the
        // cascade anyone sees, so it must stay silent even when that lone layer
        // pairs badly.
        let dir = std::env::temp_dir().join(format!("ctk-theme-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.conf.mix");
        std::fs::write(&shared, bad).unwrap();
        let written = warnings_from(|| {
            file::write_theme_selection(&shared, Scheme::Mono, Mode::Light).unwrap();
        });
        assert!(
            !faulted(&written),
            "write validation must not report a lone layer; got {written:?}"
        );

        // Both resolve entry points report the theme they hand back.
        let resolved = warnings_from(|| {
            file::resolve_theme(Some(&shared), None);
        });
        assert!(
            faulted(&resolved),
            "`resolve_theme` must report the resolved pairing, got {resolved:?}"
        );
        let with_selection = warnings_from(|| {
            file::resolve_theme_with_selection(Some(&shared), None, Scheme::Mono, Mode::Dark);
        });
        assert!(
            faulted(&with_selection),
            "`resolve_theme_with_selection` must report the resolved pairing, \
             got {with_selection:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "theme")]
    fn an_apply_theme_message_reports_the_pairing_it_commits() {
        use bevy::ecs::system::RunSystemOnce;

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        let mut spec = ThemeSpec::builtin();
        spec.colors.row_selected_text = spec.colors.row_selected;

        let warnings = warnings_from(|| {
            app.world_mut().write_message(ApplyTheme(spec));
            // Drive the exact production reader on this thread. An ordinary
            // multithreaded schedule may run it on a worker whose thread-local
            // warning sink is intentionally unarmed.
            app.world_mut()
                .run_system_once(apply_theme_requests)
                .expect("runtime theme system runs");
        });

        let state = app.world().resource::<ThemeState>();
        assert_eq!(
            state.colors.row_selected_text, state.colors.row_selected,
            "the test message must reach committed runtime state"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("row_selected_text")
                    && warning.contains("below WCAG AA")),
            "the direct runtime theme path committed a failing pairing without reporting it: \
             {warnings:?}"
        );
    }

    #[test]
    #[cfg(feature = "theme")]
    fn an_unreachable_selection_separation_emits_its_fallback_warning() {
        let warnings = warnings_from(|| {
            let _ = separated_from(ok(50.0, 0.0, 0.0), Color::BLACK, Mode::Dark, 22.0);
        });

        assert!(
            warnings.iter().any(|warning| {
                warning.contains("selection separation target 22")
                    && warning.contains("best-effort fallback")
            }),
            "the unreachable fallback must be observable, got {warnings:?}"
        );
    }

    #[test]
    fn public_contrast_helpers_reject_every_non_finite_channel_class() {
        for (name, value) in [
            ("positive infinity", f32::INFINITY),
            ("negative infinity", f32::NEG_INFINITY),
            ("NaN", f32::NAN),
        ] {
            let rgb = Color::srgb(value, 0.0, 0.0);
            assert!(
                relative_luminance(rgb).is_nan(),
                "{name} RGB acquired a plausible luminance"
            );
            assert!(
                contrast_ratio(rgb, Color::BLACK).is_nan()
                    && contrast_ratio(Color::BLACK, rgb).is_nan(),
                "{name} RGB acquired a plausible public contrast ratio"
            );
            assert!(
                !is_opaque(rgb),
                "{name} RGB was accepted as a measurable opaque colour"
            );

            let alpha = Color::srgba(1.0, 1.0, 1.0, value);
            assert!(
                contrast_ratio(alpha, Color::BLACK).is_nan()
                    && contrast_ratio(Color::BLACK, alpha).is_nan(),
                "{name} alpha acquired a plausible public contrast ratio"
            );
            assert!(
                !is_opaque(alpha),
                "{name} alpha was accepted as a measurable opaque colour"
            );
        }
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_reference_points() {
        // The two ends and the published worst case: a background at relative
        // luminance 0.179 is the only place black and white measure the same,
        // and that equal value — 4.58 — is why an AA foreground always exists.
        assert!((contrast_ratio(Color::WHITE, Color::BLACK) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(Color::WHITE, Color::WHITE) - 1.0).abs() < 0.001);
        // Chromatic reference points against white, each independently
        // published. Greys alone cannot catch a mistake in the luminance
        // coefficients, because a grey weights all three identically and any
        // set summing to 1.0 answers correctly; these three separate them.
        for (hex, expected) in [
            ("#ff0000", 4.00_f32),
            ("#008000", 5.14),
            ("#0000ff", 8.59),
            // The canonical smallest grey that clears AA on white.
            ("#767676", 4.54),
        ] {
            let c = Color::from(bevy::color::Srgba::hex(hex).expect("reference hex"));
            let measured = contrast_ratio(c, Color::WHITE);
            assert!(
                (measured - expected).abs() < 0.01,
                "{hex} on white: {measured:.3}, expected {expected}"
            );
        }
        // The linearisation cutoff, pinned by which branch it selects rather
        // than by a value. The two branches meet to within a millionth, so no
        // reference luminance catches a shift; but *which* formula ran is
        // decidable, because even one ulp from the join the branches separate by
        // 1.4e-9 to 2.1e-9 while f32 rounding at this magnitude is 2.3e-10 — a
        // factor of six. Asking which formula the answer is closer to needs no
        // tolerance at all.
        //
        // Sampled one ulp either side, so the cutoff is pinned to a single
        // representable f32 and no interval is left for a mutant to hide in: a
        // move to 0.040_459 is caught as surely as a move to 0.03928. That
        // resolution is earned, not decorative — every use of this function is
        // a `>= 4.5` decision, and pairings exist whose ratio a sub-1e-5 cutoff
        // shift walks across 4.5.
        let cutoff = 0.040_45_f32;
        let ulp = |n: i32| f32::from_bits((cutoff.to_bits() as i32 + n) as u32);
        for (c, linear, why) in [
            (ulp(-1), true, "one ulp below the cutoff must be linearised"),
            // `<=`, so the join itself is linear. Pins the comparison direction.
            (
                cutoff,
                true,
                "the cutoff itself belongs to the linear branch",
            ),
            (
                ulp(1),
                false,
                "one ulp above the cutoff must go through the power curve",
            ),
        ] {
            let got = relative_luminance(Color::srgb(c, c, c));
            let to_linear = (got - c / 12.92).abs();
            let to_power = (got - ((c + 0.055) / 1.055).powf(2.4)).abs();
            assert_eq!(
                to_linear < to_power,
                linear,
                "{why}: c={c} gave {got:.12} ({to_linear:.3e} from linear, \
                 {to_power:.3e} from the curve)"
            );
        }
        // And the consumer's view of the same fact: every use of this function
        // is a threshold comparison, so at a knife edge an arbitrarily small
        // delta flips a boolean. This colour measures 4.499999 and so fails AA;
        // moving the cutoff to 0.03928 lifts it to 4.500001 and it passes.
        // No 8-bit hex can sit between the two cutoffs; a programmatic colour
        // or an OKLCH-derived walk candidate can, and both reach this code.
        let knife_edge = Color::srgb(0.04, 0.542_412_2, 0.0);
        let measured = contrast_ratio(knife_edge, Color::WHITE);
        assert!(
            measured < AA_CONTRAST,
            "the linearisation cutoff moved: {measured:.9} now clears AA"
        );
        assert!(
            (measured - AA_CONTRAST).abs() < 1e-5,
            "this point only guards the cutoff while it stays on the knife edge; \
             it measured {measured:.9}"
        );
        // Sweep every grey rather than hardcode the crossover: the claim is
        // that *no* background defeats both ends at once, which is what lets
        // `contrast_checked` promise an answer for any wash it is handed.
        for step in 0..=1000 {
            let g = step as f32 / 1000.0;
            let grey = Color::srgb(g, g, g);
            let best = contrast_ratio(grey, Color::BLACK).max(contrast_ratio(grey, Color::WHITE));
            assert!(
                best >= AA_CONTRAST,
                "grey {step}/1000 defeats both ends: {best:.2}"
            );
        }
    }

    #[test]
    #[cfg(feature = "theme")]
    fn overriding_the_wash_re_derives_the_foreground_but_an_explicit_one_wins() {
        let file = |s: &str| -> ThemeFile {
            cosmix_config::from_conf_mix_str(s).expect("theme file parses")
        };
        // A file that moves the wash and says nothing about the foreground gets
        // a fresh, checked foreground rather than the stale derived one.
        let mut spec = ThemeSpec::builtin();
        let before = spec.colors.row_selected_text;
        let before_dim = spec.colors.row_selected_text_dim;
        spec.overlay(&file("row_selected: \"#eeeeee\"\n")).unwrap();
        assert_ne!(spec.colors.row_selected_text, before, "not re-derived");
        assert_ne!(
            spec.colors.row_selected_text_dim, before_dim,
            "dim foreground not re-derived"
        );
        assert!(
            contrast_ratio(spec.colors.row_selected_text, spec.colors.row_selected) >= AA_CONTRAST
        );
        assert!(
            contrast_ratio(spec.colors.row_selected_text_dim, spec.colors.row_selected)
                >= AA_CONTRAST
        );

        // An explicit foreground stands even when it fails — the operator's
        // call — and survives a later cascade layer that has no opinion.
        let mut spec = ThemeSpec::builtin();
        spec.overlay(&file("row_selected_text: \"#808080\"\n"))
            .unwrap();
        let explicit = spec.colors.row_selected_text;
        spec.overlay(&file("panel: \"#101010\"\n")).unwrap();
        assert_ne!(
            spec.colors.row_selected_text, explicit,
            "a file that moves the panel underneath the wash must re-derive"
        );
        let mut spec = ThemeSpec::builtin();
        spec.overlay(&file("row_selected_text: \"#808080\"\n"))
            .unwrap();
        spec.overlay(&file("track: \"#101010\"\n")).unwrap();
        assert_eq!(
            spec.colors.row_selected_text, explicit,
            "a file with no opinion on selection must not clobber an explicit value"
        );

        // The dim token follows either half of its pair unless the same layer
        // explicitly overrides it.
        let mut spec = ThemeSpec::builtin();
        spec.overlay(&file(
            "row_selected: \"#111111\"\n\
                 row_selected_text: \"#ffffff\"\n\
                 row_selected_text_dim: \"#eeeeee\"\n",
        ))
        .unwrap();
        assert_eq!(
            spec.colors.row_selected_text_dim,
            Color::srgb(238.0 / 255.0, 238.0 / 255.0, 238.0 / 255.0)
        );
    }

    #[test]
    #[cfg(feature = "theme")]
    fn either_dim_text_surface_re_derives_even_after_an_earlier_explicit_layer() {
        let file = |s: &str| -> ThemeFile {
            cosmix_config::from_conf_mix_str(s).expect("theme file parses")
        };
        let assert_pair_clears = |spec: &ThemeSpec, case: &str| {
            for (name, background) in [
                ("panel", spec.colors.panel),
                ("row_hover", spec.colors.row_hover),
            ] {
                let measured = contrast_ratio(spec.colors.text_dim, background);
                assert!(
                    measured >= AA_CONTRAST,
                    "{case}: re-derived text.dim measures {measured:.3}:1 against {name}"
                );
            }
        };

        let mut panel_only = ThemeSpec::from_scheme(Scheme::Ocean, Mode::Dark);
        let before = panel_only.colors.text_dim;
        panel_only.overlay(&file("panel: \"#505050\"\n")).unwrap();
        assert_ne!(
            panel_only.colors.text_dim, before,
            "moving panel alone must re-derive text.dim"
        );
        assert_pair_clears(&panel_only, "panel-only layer");

        let mut hover_only = ThemeSpec::from_scheme(Scheme::Ocean, Mode::Dark);
        let before = hover_only.colors.text_dim;
        hover_only
            .overlay(&file("row_hover: \"#505050\"\n"))
            .unwrap();
        assert_ne!(
            hover_only.colors.text_dim, before,
            "moving row_hover alone must re-derive text.dim"
        );
        assert_pair_clears(&hover_only, "row-hover-only layer");

        let mut layered = ThemeSpec::from_scheme(Scheme::Ocean, Mode::Dark);
        layered.overlay(&file("text_dim: \"#777777\"\n")).unwrap();
        let inherited_explicit = layered.colors.text_dim;
        layered.overlay(&file("panel: \"#505050\"\n")).unwrap();
        assert_ne!(
            layered.colors.text_dim, inherited_explicit,
            "a later single-surface layer must re-derive an inherited explicit text.dim"
        );
        assert_pair_clears(&layered, "layer after an explicit text.dim");
    }

    #[test]
    fn scheme_changes_the_accent_hue_and_mode_flips_lightness() {
        // Crimson keeps the punchy accent from the web (oklch 63% .23 25).
        let crimson = ThemeSpec::from_scheme(Scheme::Crimson, Mode::Dark);
        assert_eq!(crimson.colors.control_active, ok(63., 0.23, 25.).color());
        assert_ne!(
            crimson.colors.surface,
            ThemeSpec::builtin().colors.surface,
            "a different scheme is a different surface"
        );
        // Light mode raises the surface lightness (dark 12% → light 98%).
        let light = ThemeSpec::from_scheme(Scheme::Ocean, Mode::Light);
        assert_eq!(light.colors.surface, ok(98., 0.008, 220.).color());
    }

    #[test]
    fn mono_is_greyscale_but_meters_stay_coloured() {
        let mono = ThemeSpec::from_scheme(Scheme::Mono, Mode::Dark);
        assert_eq!(mono.colors.surface, ok(10., 0.0, 0.0).color());
        assert_eq!(mono.colors.control_active, ok(85., 0.0, 0.0).color());
        // Status meters are scheme-invariant — still green/amber/red.
        assert_eq!(mono.colors.meter_green, ok(70., 0.15, 145.).color());
        assert_eq!(mono.colors.meter_red, ok(70., 0.18, 25.).color());
    }

    #[test]
    fn scheme_names_round_trip() {
        for s in Scheme::ALL {
            assert_eq!(Scheme::from_name(s.name()), Some(s));
        }
        assert_eq!(Scheme::from_name("nope"), None);
    }

    #[test]
    fn runtime_apply_advances_only_for_a_changed_colour_spec() {
        let mut theme = UiTheme::default();
        let mut state = ThemeState::default();
        let ocean = ThemeSpec::builtin();
        assert!(apply_theme(&mut theme, &mut state, &ocean));
        assert_eq!(state.revision, 1);
        assert!(!apply_theme(&mut theme, &mut state, &ocean));
        assert_eq!(state.revision, 1);

        let forest = ThemeSpec::from_scheme(Scheme::Forest, Mode::Light);
        assert!(apply_theme(&mut theme, &mut state, &forest));
        assert_eq!(state.scheme, Scheme::Forest);
        assert_eq!(state.mode, Mode::Light);
        assert_eq!(state.revision, 2);
        assert_eq!(
            ctk_color(&theme, &tokens::ROW_SELECTED),
            forest.colors.row_selected
        );
        assert_eq!(
            ctk_color(&theme, &tokens::ROW_SELECTED_TEXT_DIM),
            forest.colors.row_selected_text_dim
        );
    }

    #[test]
    fn authored_font_scaling_is_non_cumulative_across_live_changes() {
        let authored = FontSize::Px(18.0);
        let first = scale_authored_font_size(authored, 13.333);
        let second = scale_authored_font_size(authored, 20.0);

        let FontSize::Px(first_px) = first else {
            panic!("authored px stays px");
        };
        let FontSize::Px(second_px) = second else {
            panic!("authored px stays px");
        };
        assert!((first_px - 18.0 * 13.333 / 13.0).abs() < 0.00001);
        assert!((second_px - 18.0 * 20.0 / 13.0).abs() < 0.00001);
        assert_ne!(
            second,
            scale_authored_font_size(first, 20.0),
            "the second live apply must use the authored size, not the first result"
        );
    }

    #[test]
    fn typography_opt_out_skips_stamping_and_management() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 13.333;
        }
        let opted_out_font = TextFont::from_font_size(12.0);
        let opted_out_source = opted_out_font.font.clone();
        let opted_out = app
            .world_mut()
            .spawn((opted_out_font, CtkTypographyOptOut))
            .id();
        let managed = app.world_mut().spawn(TextFont::from_font_size(12.0)).id();

        app.update();

        assert_eq!(
            app.world().get::<TextFont>(opted_out).unwrap().font,
            opted_out_source
        );
        assert!(
            app.world().get::<ManagedTypography>(opted_out).is_none(),
            "opted-out text must never become managed"
        );
        assert_eq!(
            app.world().get::<TextFont>(managed).unwrap().font,
            FontSource::SansSerif
        );
        assert!(app.world().get::<ManagedTypography>(managed).is_some());
    }

    #[test]
    fn a_last_good_family_that_no_longer_maps_is_not_reported_as_in_effect() {
        // A mapping can be lost without any theme change — dropping the last
        // strong handle to a font asset rebuilds the collection. Reporting the
        // family CTK once resolved, while the generic sans now points somewhere
        // else, would make this resource lie to whoever reads it.
        const GONE: &str = "CTK Test Vanished Family 4f2c0b19";
        const MISSING: &str = "CTK Test Missing Family 9d3e51aa";

        let mut app = App::new();
        app.init_resource::<FontCx>()
            .add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some(GONE.to_string());
            typography.fallback = TypographyFallback::LastKnownGood;
        }
        let mut spec = ThemeSpec::builtin();
        spec.typography.family = MISSING.to_string();
        app.world_mut().write_message(ApplyTheme(spec));

        app.update();

        let typography = app.world().resource::<CtkTypography>();
        assert_eq!(
            typography.effective_family, None,
            "a family the collection no longer maps must be dropped, not kept"
        );
        assert_eq!(typography.fallback, TypographyFallback::Embedded);
    }

    #[test]
    fn the_configured_size_keeps_applying_after_a_mapping_is_lost() {
        // The size is a separate theme value from the family. Text that CTK
        // already manages must not freeze at whatever size was in force when
        // the mapping was last good.
        // Drives the stamping loop directly: no `FontCx`, so the mapping state
        // is exactly what this test sets rather than whatever the host's font
        // collection happens to hold.
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0; // exactly 2x the authoring baseline
        }
        let text = app.world_mut().spawn(TextFont::from_font_size(10.0)).id();
        app.update();
        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(20.0),
            "the mapping is still claimed at this point"
        );

        // The mapping goes away — a rebuilt collection, not a theme change —
        // and the base size moves at the same time.
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = None;
            typography.fallback = TypographyFallback::Embedded;
            typography.body_px = 39.0; // 3x
        }
        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(30.0),
            "the new base size still reaches managed text"
        );
        assert!(
            matches!(
                app.world().get::<TextFont>(text).unwrap().font,
                FontSource::SansSerif
            ),
            "an already-stamped source is kept, not unwound to the ASCII-only \
             embedded fallback"
        );
    }

    #[test]
    fn missing_family_warns_and_leaves_new_text_source_untouched() {
        const MISSING: &str = "CTK Test Missing Family 7a1049e7";

        let mut app = App::new();
        app.init_resource::<FontCx>()
            .add_plugins(CtkThemePlugin::default());
        let original = TextFont::from_font_size(13.0);
        let original_source = original.font.clone();
        let text = app.world_mut().spawn(original).id();
        let mut spec = ThemeSpec::builtin();
        spec.typography.family = MISSING.to_string();
        app.world_mut().write_message(ApplyTheme(spec));

        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font,
            original_source,
            "first-start failure must preserve the embedded font source"
        );
        assert!(
            app.world().get::<ManagedTypography>(text).is_some(),
            "failed text remains eligible for a later valid live theme"
        );
        let typography = app.world().resource::<CtkTypography>();
        assert_eq!(typography.effective_family, None);
        assert_eq!(typography.fallback, TypographyFallback::Embedded);
        let warning = typography
            .last_warning
            .as_deref()
            .expect("missing family records its emitted warning");
        assert!(warning.contains(MISSING));
    }

    #[test]
    fn an_unresolved_family_is_retried_rather_than_cached_against_the_revision() {
        const MISSING: &str = "CTK Test Missing Family 0f52c9d1";

        let mut app = App::new();
        app.init_resource::<FontCx>()
            .add_plugins(CtkThemePlugin::default());
        let text = app.world_mut().spawn(TextFont::from_font_size(13.0)).id();
        let mut spec = ThemeSpec::builtin();
        spec.typography.family = MISSING.to_string();
        app.world_mut().write_message(ApplyTheme(spec));
        app.update();
        assert_eq!(
            app.world().resource::<CtkTypography>().effective_family,
            None
        );

        // Change the request WITHOUT bumping the theme revision. If the miss
        // were cached against the revision, the resolver would return early and
        // never look at the new family. This assertion holds on any host,
        // including one with no system fonts at all — which is precisely the
        // host the fallback path exists for.
        const OTHER_MISSING: &str = "CTK Test Missing Family 5b3ea472";
        app.world_mut()
            .resource_mut::<ThemeState>()
            .typography
            .family = OTHER_MISSING.to_string();

        app.update();

        assert_eq!(
            app.world().resource::<CtkTypography>().requested_family,
            OTHER_MISSING,
            "an unresolved family must be re-examined on a later pass"
        );

        // Where the host does have fonts, prove the retry can also succeed.
        let available = {
            let mut font_cx = app.world_mut().resource_mut::<FontCx>();
            let first = font_cx.collection.family_names().next().map(str::to_string);
            first
        };
        let Some(available) = available else {
            return;
        };
        app.world_mut()
            .resource_mut::<ThemeState>()
            .typography
            .family = available;

        app.update();

        let typography = app.world().resource::<CtkTypography>();
        assert!(
            typography.effective_family.is_some(),
            "a family that becomes available must be picked up on a later pass"
        );
        assert_eq!(typography.fallback, TypographyFallback::Requested);
        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font,
            FontSource::SansSerif
        );
    }

    #[test]
    fn a_font_source_reassignment_is_restored_without_rescaling_the_size() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0; // exactly 2x the authoring baseline
        }
        let text = app.world_mut().spawn(TextFont::from_font_size(10.0)).id();
        app.update();
        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(20.0)
        );

        // Somebody reassigns only the SOURCE. Treating that as a size write
        // would re-adopt the already-scaled 20 and double it on every pass.
        for _ in 0..4 {
            app.world_mut().get_mut::<TextFont>(text).unwrap().font = FontSource::Monospace;
            app.update();
            let font = app.world().get::<TextFont>(text).unwrap();
            assert_eq!(
                font.font_size,
                FontSize::Px(20.0),
                "a source reassignment must not rescale the size"
            );
            assert_eq!(
                font.font,
                FontSource::SansSerif,
                "CTK restores its own source to a managed entity"
            );
        }
    }

    #[test]
    fn a_size_written_before_any_mapping_existed_is_still_adopted() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        let text = app.world_mut().spawn(TextFont::from_font_size(12.0)).id();

        // No mapping yet: the entity becomes managed but nothing is stamped.
        app.update();
        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(12.0)
        );

        // The app changes its mind while unmapped.
        app.world_mut().get_mut::<TextFont>(text).unwrap().font_size = FontSize::Px(18.0);

        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0;
        }

        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(36.0),
            "the mapping must scale the size in force, not the one first seen"
        );
    }

    #[test]
    fn a_non_finite_authored_size_is_left_alone_rather_than_churned() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0;
        }
        let text = app
            .world_mut()
            .spawn(TextFont::from_font_size(f32::NAN))
            .id();

        app.update();
        app.update();

        assert!(
            app.world().get::<ManagedTypography>(text).is_none(),
            "an unmanageable size must not be taken under management"
        );
        let FontSize::Px(size) = app.world().get::<TextFont>(text).unwrap().font_size else {
            panic!("px stays px");
        };
        assert!(
            size.is_nan(),
            "the author's value is left exactly as spawned"
        );
    }

    #[test]
    fn a_corrected_non_finite_size_joins_management_on_the_next_write() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0; // exactly 2x the authoring baseline
        }
        let text = app
            .world_mut()
            .spawn(TextFont::from_font_size(f32::NAN))
            .id();
        app.update();
        assert!(
            app.world().get::<ManagedTypography>(text).is_none(),
            "the entity starts outside management"
        );

        app.world_mut().get_mut::<TextFont>(text).unwrap().font_size = FontSize::Px(10.0);
        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(20.0),
            "correcting the size must re-enrol the entity rather than exclude it for life"
        );
    }

    #[test]
    fn an_external_font_size_write_is_adopted_as_the_new_authored_size() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 26.0; // exactly 2x the authoring baseline
        }
        let text = app.world_mut().spawn(TextFont::from_font_size(10.0)).id();

        app.update();
        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(20.0)
        );

        // Somebody else rewrites the size. That write is the new intent, so the
        // next pass scales from 30, not from the original 10.
        app.world_mut().get_mut::<TextFont>(text).unwrap().font_size = FontSize::Px(30.0);

        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            FontSize::Px(60.0),
            "an external write must be re-adopted, not silently reverted"
        );
        assert_eq!(
            app.world()
                .get::<ManagedTypography>(text)
                .unwrap()
                .authored_size,
            FontSize::Px(30.0)
        );
    }

    #[test]
    fn a_settled_entity_is_not_rewritten_on_an_idle_pass() {
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 13.0;
        }
        let text = app.world_mut().spawn(TextFont::from_font_size(12.0)).id();
        app.update();
        app.update();

        use bevy::ecs::change_detection::DetectChanges;
        let settled = app
            .world()
            .entity(text)
            .get_ref::<TextFont>()
            .unwrap()
            .last_changed();

        app.update();

        let after = app
            .world()
            .entity(text)
            .get_ref::<TextFont>()
            .unwrap()
            .last_changed();
        assert_eq!(
            settled, after,
            "reconciling a settled entity must not re-trigger text rerender every frame"
        );
    }

    #[test]
    fn an_out_of_range_body_px_is_clamped_before_it_reaches_text_layout() {
        // A hand-built spec bypasses file validation entirely, so the consumer
        // is the backstop: `13333` must not reach the glyph atlas.
        let mut app = App::new();
        app.init_resource::<FontCx>()
            .add_plugins(CtkThemePlugin::default());
        let mut spec = ThemeSpec::builtin();
        spec.typography.body_px = 13_333.0;
        app.world_mut().write_message(ApplyTheme(spec));

        app.update();

        assert_eq!(
            app.world().resource::<CtkTypography>().body_px,
            MAX_BODY_PX,
            "an absurd configured size is clamped, not handed to text layout"
        );
    }

    #[test]
    fn a_body_px_written_straight_onto_the_resource_is_still_clamped() {
        // `CtkTypography` is public and writable, so the cascade's bounds are
        // not the only ingress. The size that reaches a `TextFont` must be
        // bounded regardless of how it got into the resource.
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        {
            let mut typography = app.world_mut().resource_mut::<CtkTypography>();
            typography.effective_family = Some("test-sans".to_string());
            typography.body_px = 13_333.0;
        }
        let text = app.world_mut().spawn(TextFont::from_font_size(13.0)).id();

        app.update();

        assert_eq!(
            app.world().get::<TextFont>(text).unwrap().font_size,
            scale_authored_font_size(FontSize::Px(13.0), MAX_BODY_PX)
        );
        assert_eq!(
            app.world().resource::<CtkTypography>().body_px,
            MAX_BODY_PX,
            "introspection must report the size actually in force, not the \
             out-of-range one that was written"
        );
    }

    #[test]
    fn a_non_finite_body_px_falls_back_to_the_default() {
        let mut app = App::new();
        app.init_resource::<FontCx>()
            .add_plugins(CtkThemePlugin::default());
        let mut spec = ThemeSpec::builtin();
        spec.typography.body_px = f32::NAN;
        app.world_mut().write_message(ApplyTheme(spec));

        app.update();

        assert_eq!(
            app.world().resource::<CtkTypography>().body_px,
            DEFAULT_BODY_PX
        );
    }
}

#[cfg(all(test, feature = "theme"))]
mod theme_file_tests {
    use super::*;
    use cosmix_design::{
        ButtonCellKey, ButtonSize, ButtonVariant, DesignCompileOutcome, InteractionState,
        EMBEDDED_DEFAULT_SOURCE,
    };
    use tempfile::TempDir;

    use crate::design::{CtkDesign, CtkDesignStatus};
    use crate::widgets::CtkWidgetsPlugin;

    fn hexc(s: &str) -> Color {
        Color::from(bevy::color::Srgba::hex(s).unwrap())
    }

    fn design_source_with_height(height: f32) -> String {
        EMBEDDED_DEFAULT_SOURCE.replacen(
            "\"button.height.md\": { kind: \"px\", value: 28.0 }",
            &format!("\"button.height.md\": {{ kind: \"px\", value: {height:.1} }}"),
            1,
        )
    }

    fn disk_design_app(
        shared_path: std::path::PathBuf,
        app_dir: Option<std::path::PathBuf>,
    ) -> App {
        let reload = ThemeReloadSignal::default();
        reload.request_reload();
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .insert_resource(ThemeRuntimeConfig {
                shared_path,
                app_config_dir: app_dir,
            })
            .init_resource::<ThemeLayerLastGood>()
            .insert_resource(reload)
            .add_message::<ApplyTheme>()
            .add_systems(Update, (reload_theme_files, apply_theme_requests).chain());
        app
    }

    fn resting_default_cell(app: &App) -> &cosmix_design::ResolvedButtonCell {
        app.world()
            .resource::<CtkDesign>()
            .button_cell(ButtonCellKey {
                variant: ButtonVariant::Default,
                size: ButtonSize::Md,
                interaction: InteractionState::Resting,
                focus_visible: false,
            })
            .unwrap()
    }

    #[test]
    fn shared_design_section_is_selected_on_first_update() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        let mut app = App::new();
        app.add_plugins((
            CtkThemePlugin {
                app_config_dir: None,
                shared_path: shared_path.clone(),
            },
            CtkWidgetsPlugin,
        ));

        app.update();

        assert_eq!(resting_default_cell(&app).height, 31.0);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            shared_path.to_string_lossy()
        );
    }

    #[test]
    fn app_design_section_overrides_shared_design_section() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join("shared").join(THEME_FILE);
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        let app_path = app_dir.join(THEME_FILE);
        std::fs::write(&app_path, design_source_with_height(33.0)).unwrap();
        let mut app = disk_design_app(shared_path, Some(app_dir));

        app.update();

        assert_eq!(resting_default_cell(&app).height, 33.0);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            app_path.to_string_lossy()
        );
    }

    #[test]
    fn selection_only_file_keeps_the_embedded_design_source() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, "{ scheme: \"ocean\", mode: \"light\" }").unwrap();
        let mut app = disk_design_app(shared_path, None);
        let before = app.world().resource::<CtkDesign>().revision();

        app.update();

        assert_eq!(app.world().resource::<CtkDesign>().revision(), before);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            "ctk:embedded-default"
        );
    }

    #[test]
    fn unchanged_disk_bytes_do_not_advance_the_design_revision() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        let source = design_source_with_height(31.0);
        std::fs::write(&shared_path, &source).unwrap();
        let mut app = disk_design_app(shared_path.clone(), None);
        app.update();
        let revision = app.world().resource::<CtkDesign>().revision();

        std::fs::write(&shared_path, &source).unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(app.world().resource::<CtkDesign>().revision(), revision);
    }

    #[test]
    fn removed_design_file_restores_the_embedded_source() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(shared_path.clone(), None);
        app.update();
        assert_eq!(resting_default_cell(&app).height, 31.0);

        std::fs::remove_file(&shared_path).unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(resting_default_cell(&app).height, 28.0);
        let status = app.world().resource::<CtkDesignStatus>();
        assert_eq!(status.source_identity().as_str(), "ctk:embedded-default");
        assert!(status.last_error().is_none());
    }

    #[test]
    fn broken_disk_edit_keeps_last_good_and_fixed_bytes_advance_once() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(shared_path.clone(), None);
        app.update();
        let last_good = app.world().resource::<CtkDesign>().revision();

        std::fs::write(&shared_path, "design: nope").unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();
        assert_eq!(app.world().resource::<CtkDesign>().revision(), last_good);
        let status = app.world().resource::<CtkDesignStatus>();
        assert_eq!(
            status.last_compile().map(|compile| compile.outcome),
            Some(DesignCompileOutcome::Fatal)
        );
        assert!(status.last_error().is_some());

        let fixed = design_source_with_height(34.0);
        std::fs::write(&shared_path, &fixed).unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();
        let fixed_revision = app.world().resource::<CtkDesign>().revision();
        assert_eq!(fixed_revision.unwrap().get(), last_good.unwrap().get() + 1);
        assert_eq!(resting_default_cell(&app).height, 34.0);

        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();
        assert_eq!(
            app.world().resource::<CtkDesign>().revision(),
            fixed_revision
        );
    }

    #[test]
    fn empty_mid_save_snapshot_keeps_the_previous_palette_and_design() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join("shared").join(THEME_FILE);
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&shared_path, "{ surface: \"#ffffff\" }").unwrap();
        let app_path = app_dir.join(THEME_FILE);
        std::fs::write(&app_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(shared_path, Some(app_dir));
        app.update();
        let palette = app.world().resource::<ThemeState>().colors.surface;
        let revision = app.world().resource::<CtkDesign>().revision();
        assert_eq!(resting_default_cell(&app).height, 31.0);
        assert_ne!(palette, hexc("#ffffff"));

        std::fs::write(&app_path, []).unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(app.world().resource::<ThemeState>().colors.surface, palette);
        assert_eq!(app.world().resource::<CtkDesign>().revision(), revision);
        assert_eq!(resting_default_cell(&app).height, 31.0);
    }

    #[test]
    fn present_nil_design_is_rejected_and_keeps_last_good() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(shared_path.clone(), None);
        app.update();
        let last_good = app.world().resource::<CtkDesign>().revision();

        std::fs::write(&shared_path, "{ design: nil }").unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(app.world().resource::<CtkDesign>().revision(), last_good);
        let status = app.world().resource::<CtkDesignStatus>();
        assert_eq!(
            status.last_compile().map(|compile| compile.outcome),
            Some(DesignCompileOutcome::Fatal)
        );
        assert!(status.last_error().is_some());
        assert_eq!(
            status.source_identity().as_str(),
            shared_path.to_string_lossy()
        );
    }

    #[test]
    fn invalid_app_palette_does_not_mask_shared_design_and_logs_once() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join("shared").join(THEME_FILE);
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&shared_path, design_source_with_height(31.0)).unwrap();
        std::fs::write(app_dir.join(THEME_FILE), "{ scheme: 7 }").unwrap();
        let mut app = disk_design_app(shared_path.clone(), Some(app_dir));

        app.update();

        assert_eq!(resting_default_cell(&app).height, 31.0);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            shared_path.to_string_lossy()
        );
        let reload = app.world().resource::<ThemeReloadSignal>();
        assert_eq!(reload.logged_failure_count(), 1);

        reload.request_reload();
        app.update();
        assert_eq!(
            app.world()
                .resource::<ThemeReloadSignal>()
                .logged_failure_count(),
            1,
            "the identical bad palette fingerprint must not log twice"
        );
    }

    #[test]
    fn rejected_palette_retains_its_design_until_a_valid_layer_withdraws_it() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join("shared").join(THEME_FILE);
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&shared_path, design_source_with_height(30.0)).unwrap();
        let app_path = app_dir.join(THEME_FILE);
        std::fs::write(&app_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(shared_path.clone(), Some(app_dir));
        app.update();
        let revision = app.world().resource::<CtkDesign>().revision();
        let palette = app.world().resource::<ThemeState>().clone();
        assert_eq!(resting_default_cell(&app).height, 31.0);

        std::fs::write(&app_path, "{ scheme: 7 }").unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(app.world().resource::<CtkDesign>().revision(), revision);
        assert_eq!(resting_default_cell(&app).height, 31.0);
        assert_eq!(app.world().resource::<ThemeState>().scheme, palette.scheme);
        assert_eq!(app.world().resource::<ThemeState>().mode, palette.mode);
        assert_eq!(app.world().resource::<ThemeState>().colors, palette.colors);
        assert_eq!(
            app.world()
                .resource::<ThemeReloadSignal>()
                .logged_failure_count(),
            1
        );

        std::fs::write(&app_path, "{ scheme: \"forest\", mode: \"dark\" }").unwrap();
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();

        assert_eq!(resting_default_cell(&app).height, 30.0);
        assert_eq!(app.world().resource::<ThemeState>().scheme, Scheme::Forest);
        assert_eq!(app.world().resource::<ThemeState>().mode, Mode::Dark);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            shared_path.to_string_lossy()
        );
        assert_eq!(
            app.world()
                .resource::<CtkDesign>()
                .revision()
                .unwrap()
                .get(),
            revision.unwrap().get() + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_not_found_open_error_keeps_the_last_good_layer() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_path = app_dir.join(THEME_FILE);
        std::fs::write(&app_path, design_source_with_height(31.0)).unwrap();
        let mut app = disk_design_app(temp.path().join("missing-shared"), Some(app_dir.clone()));
        app.update();
        let revision = app.world().resource::<CtkDesign>().revision();
        let palette = app.world().resource::<ThemeState>().clone();

        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&app_path).is_ok() {
            std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        app.world().resource::<ThemeReloadSignal>().request_reload();
        app.update();
        std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(app.world().resource::<CtkDesign>().revision(), revision);
        assert_eq!(resting_default_cell(&app).height, 31.0);
        assert_eq!(app.world().resource::<ThemeState>().colors, palette.colors);
        assert_eq!(
            app.world()
                .resource::<ThemeReloadSignal>()
                .logged_failure_count(),
            1
        );
    }

    #[test]
    fn non_map_app_layer_is_rejected_once_and_does_not_mask_shared_design() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join("shared").join(THEME_FILE);
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(shared_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&shared_path, design_source_with_height(32.0)).unwrap();
        std::fs::write(app_dir.join(THEME_FILE), "[1, 2, 3]").unwrap();
        let mut app = disk_design_app(shared_path.clone(), Some(app_dir));

        app.update();

        assert_eq!(resting_default_cell(&app).height, 32.0);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            shared_path.to_string_lossy()
        );
        let reload = app.world().resource::<ThemeReloadSignal>();
        assert_eq!(reload.logged_failure_count(), 1);
        reload.request_reload();
        app.update();
        assert_eq!(
            app.world()
                .resource::<ThemeReloadSignal>()
                .logged_failure_count(),
            1
        );
    }

    #[test]
    fn oversized_theme_layer_is_rejected_before_parsing() {
        let temp = TempDir::new().unwrap();
        let shared_path = temp.path().join(THEME_FILE);
        std::fs::write(&shared_path, vec![b' '; MAX_THEME_FILE_BYTES as usize + 1]).unwrap();
        let mut app = disk_design_app(shared_path, None);

        app.update();

        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            "ctk:embedded-default"
        );
        assert_eq!(
            app.world()
                .resource::<ThemeReloadSignal>()
                .logged_failure_count(),
            1
        );
    }

    #[test]
    fn directory_watcher_coalesces_a_burst_into_one_pending_reload() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(THEME_FILE);
        let reload = ThemeReloadSignal::default();
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(1);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let _watcher = theme_file_watcher(vec![path.clone()], reload.clone(), wake).unwrap();

        for height in 29..34 {
            std::fs::write(&path, design_source_with_height(height as f32)).unwrap();
        }
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("directory watcher did not deliver the write burst");

        assert_eq!(reload.pending_reload_count(), 1);
    }

    #[test]
    fn watcher_event_filter_ignores_reads_and_accepts_content_invalidations() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, Flag, MetadataKind, ModifyKind,
            RemoveKind, RenameMode,
        };
        use notify::{Event, EventKind};

        let target = std::path::PathBuf::from("/tmp/ctk-filter/theme.conf.mix");
        let paths = ThemeWatchPaths {
            targets: vec![target.clone()],
            parents: vec![target.parent().unwrap().to_path_buf()],
        };
        let event = |kind| Event::new(kind).add_path(target.clone());

        assert!(!theme_event_requests_reload(
            &event(EventKind::Access(AccessKind::Open(AccessMode::Read))),
            &paths
        ));
        assert!(!theme_event_requests_reload(
            &event(EventKind::Access(AccessKind::Close(AccessMode::Read))),
            &paths
        ));
        assert!(!theme_event_requests_reload(
            &event(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any))),
            &paths
        ));
        assert!(!theme_event_requests_reload(
            &event(EventKind::Create(CreateKind::File)),
            &paths
        ));
        assert!(!theme_event_requests_reload(
            &event(EventKind::Modify(ModifyKind::Data(DataChange::Any))),
            &paths
        ));
        assert!(!theme_event_requests_reload(
            &event(EventKind::Modify(ModifyKind::Name(RenameMode::From))),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &event(EventKind::Modify(ModifyKind::Name(RenameMode::To))),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &event(EventKind::Modify(ModifyKind::Name(RenameMode::Both))),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &event(EventKind::Access(AccessKind::Close(AccessMode::Write))),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &event(EventKind::Remove(RemoveKind::File)),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &Event::new(EventKind::Other).set_flag(Flag::Rescan),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &Event::new(EventKind::Any),
            &paths
        ));
        assert!(theme_event_requests_reload(
            &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .add_path(target.parent().unwrap().to_path_buf()),
            &paths
        ));
    }

    #[test]
    fn watcher_does_not_request_another_reload_for_its_own_read() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(THEME_FILE);
        std::fs::write(&path, design_source_with_height(29.0)).unwrap();
        let reload = ThemeReloadSignal::default();
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(2);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let watcher = theme_file_watcher(vec![path.clone()], reload.clone(), wake).unwrap();
        let mut app = disk_design_app(path.clone(), None);
        app.insert_resource(reload.clone()).insert_resource(watcher);

        std::fs::write(&path, design_source_with_height(30.0)).unwrap();
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("directory watcher did not deliver the write");
        assert!(
            woke_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "the write burst should stay coalesced while its reload is pending"
        );
        app.update();
        let completed_count = reload.pending_reload_count();

        // A very heavily loaded CI worker could delay an erroneous event past
        // this negative window and false-pass. The committed-write and parent-
        // replacement positive tests are the load-bearing watcher coverage.
        assert!(
            woke_rx
                .recv_timeout(std::time::Duration::from_millis(250))
                .is_err(),
            "the reload's own file read requested another reload"
        );
        assert_eq!(reload.pending_reload_count(), completed_count);
    }

    #[test]
    fn parent_rename_requests_reload_and_reinstalls_without_focus() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("watched");
        let moved = temp.path().join("moved");
        let path = parent.join(THEME_FILE);
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&path, "{ scheme: \"ocean\" }").unwrap();
        let reload = ThemeReloadSignal::default();
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(2);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let watcher = theme_file_watcher(vec![path], reload.clone(), wake).unwrap();
        let initial_generation = watcher.watch_generation();
        let mut app = disk_design_app(parent.join(THEME_FILE), None);
        app.insert_resource(reload.clone()).insert_resource(watcher);

        std::fs::rename(&parent, &moved).unwrap();
        std::fs::rename(&moved, &parent).unwrap();
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("renaming the watched parent did not request a reload");
        assert_eq!(reload.pending_reload_count(), 1);

        app.update();
        assert!(
            app.world()
                .resource::<ThemeFileWatcher>()
                .watch_generation()
                > initial_generation,
            "parent invalidation did not force the watch to be reinstalled"
        );
    }

    #[test]
    fn relative_app_directory_is_absolutised_for_reads_and_watch_matches() {
        let current = std::env::current_dir().unwrap();
        let temp = tempfile::Builder::new()
            .prefix("ctk-relative-theme-")
            .tempdir_in(&current)
            .unwrap();
        let app_dir = temp.path().join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let relative_app = app_dir.strip_prefix(&current).unwrap().to_path_buf();
        assert!(relative_app.is_relative());
        let relative_target = relative_app.join(THEME_FILE);
        std::fs::write(&relative_target, "{ scheme: \"ocean\" }").unwrap();

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::new(Some(relative_app)));
        let config = app.world().resource::<ThemeRuntimeConfig>();
        assert!(config.shared_path.is_absolute());
        assert!(config.app_config_dir.as_ref().unwrap().is_absolute());

        let reload = ThemeReloadSignal::default();
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(1);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let watcher =
            theme_file_watcher(vec![relative_target.clone()], reload.clone(), wake).unwrap();
        assert!(watcher.targets.iter().all(|target| target.is_absolute()));

        std::fs::write(&relative_target, "{ scheme: \"forest\" }").unwrap();
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("relative configured target did not match its absolute notify event");
        assert_eq!(reload.pending_reload_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_config_symlink_swap_moves_the_watch_and_applies_the_new_design() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let real1 = temp.path().join("real1");
        let real2 = temp.path().join("real2");
        std::fs::create_dir_all(&real1).unwrap();
        std::fs::create_dir_all(&real2).unwrap();
        std::fs::write(real1.join(THEME_FILE), design_source_with_height(31.0)).unwrap();
        std::fs::write(real2.join(THEME_FILE), design_source_with_height(35.0)).unwrap();
        let configured_dir = temp.path().join("cfg");
        symlink(&real1, &configured_dir).unwrap();
        let configured_path = configured_dir.join(THEME_FILE);

        let reload = ThemeReloadSignal::default();
        let (woke_tx, woke_rx) = std::sync::mpsc::sync_channel(2);
        let wake = std::sync::Arc::new(move || {
            let _ = woke_tx.try_send(());
        });
        let watcher =
            theme_file_watcher(vec![configured_path.clone()], reload.clone(), wake).unwrap();
        let initial_generation = watcher.watch_generation();
        reload.request_reload();
        let mut app = disk_design_app(configured_path.clone(), None);
        app.insert_resource(reload.clone()).insert_resource(watcher);
        app.update();
        assert_eq!(resting_default_cell(&app).height, 31.0);

        let replacement = temp.path().join("cfg.next");
        symlink(&real2, &replacement).unwrap();
        std::fs::rename(&replacement, &configured_dir).unwrap();
        woke_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("atomic config symlink swap did not request a reload");
        app.update();

        assert_eq!(resting_default_cell(&app).height, 35.0);
        assert_eq!(
            app.world()
                .resource::<CtkDesignStatus>()
                .source_identity()
                .as_str(),
            configured_path.to_string_lossy(),
            "the configured lexical path remains the source identity"
        );
        let watcher = app.world().resource::<ThemeFileWatcher>();
        assert!(watcher.watch_generation() > initial_generation);
        let event_paths = watcher
            .event_paths
            .read()
            .expect("CTK theme watcher paths poisoned");
        assert!(event_paths.parents.contains(&real2));
        assert!(!event_paths.parents.contains(&real1));
    }

    #[test]
    fn focus_reload_reinstalls_a_watch_after_parent_recreation() {
        let temp = TempDir::new().unwrap();
        let watched_parent = temp.path().join("recreated");
        let shared_path = watched_parent.join(THEME_FILE);
        std::fs::create_dir_all(&watched_parent).unwrap();
        std::fs::write(&shared_path, "{ scheme: \"ocean\" }").unwrap();
        let mut app = App::new();
        app.add_plugins(CtkThemePlugin {
            app_config_dir: None,
            shared_path,
        });
        app.update();
        let first_generation = app
            .world()
            .resource::<ThemeFileWatcher>()
            .watch_generation();

        std::fs::remove_dir_all(&watched_parent).unwrap();
        app.world_mut().write_message(WindowFocused {
            window: bevy::prelude::Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();

        assert!(watched_parent.is_dir());
        assert!(
            app.world()
                .resource::<ThemeFileWatcher>()
                .watch_generation()
                > first_generation,
            "the focus backstop did not replace the dead directory watch"
        );
    }

    #[test]
    fn overlay_applies_only_present_fields() {
        let mut spec = ThemeSpec::builtin();
        let base_panel = spec.colors.panel;
        let base_knob = spec.metrics.knob_size;
        let file = ThemeFile {
            surface: Some("#ffffff".into()),
            fader_width: Some(99.0),
            ..Default::default()
        };
        spec.overlay(&file).unwrap();
        assert_eq!(spec.colors.surface, hexc("#ffffff"));
        assert_eq!(spec.metrics.fader_width, 99.0);
        assert_eq!(
            spec.colors.panel, base_panel,
            "absent colour keeps built-in"
        );
        assert_eq!(
            spec.metrics.knob_size, base_knob,
            "absent metric keeps built-in"
        );
    }

    #[test]
    fn overlay_rejects_an_out_of_range_body_px() {
        for bad in [
            0.0,
            MIN_BODY_PX - 0.1,
            MAX_BODY_PX + 0.1,
            13_333.0,
            f32::NAN,
        ] {
            let mut spec = ThemeSpec::builtin();
            let baseline = spec.typography.body_px;
            let file = ThemeFile {
                typography: Some(TypographyFile {
                    body_px: Some(bad),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let err = spec
                .overlay(&file)
                .expect_err("an out-of-range body_px is a theme-file error");
            assert!(err.contains("body_px"), "error names the field: {err}");
            assert_eq!(
                spec.typography.body_px, baseline,
                "a rejected file must not partially apply"
            );
        }

        let mut spec = ThemeSpec::builtin();
        let file = ThemeFile {
            typography: Some(TypographyFile {
                body_px: Some(MAX_BODY_PX),
                ..Default::default()
            }),
            ..Default::default()
        };
        spec.overlay(&file).expect("the bound itself is accepted");
        assert_eq!(spec.typography.body_px, MAX_BODY_PX);
    }

    #[test]
    fn overlay_rejects_a_malformed_colour() {
        let mut spec = ThemeSpec::builtin();
        let before = spec.clone();
        let file = ThemeFile {
            scheme: Some("forest".into()),
            surface: Some("nope".into()),
            ..Default::default()
        };
        assert!(spec.overlay(&file).is_err());
        assert_eq!(spec.scheme, before.scheme);
        assert_eq!(spec.mode, before.mode);
        assert_eq!(spec.colors, before.colors);
        assert_eq!(spec.metrics, before.metrics);
    }

    #[test]
    fn overlay_scheme_reselects_the_palette_then_hex_wins() {
        let mut spec = ThemeSpec::builtin();
        let file = ThemeFile {
            scheme: Some("forest".into()),
            mode: Some("dark".into()),
            // A per-token hex override applied ON TOP of the reselected scheme.
            surface: Some("#010203".into()),
            ..Default::default()
        };
        spec.overlay(&file).unwrap();
        assert_eq!(spec.scheme, Scheme::Forest);
        // control.active came from Forest (the hex only overrode surface).
        assert_eq!(
            spec.colors.control_active,
            ThemeSpec::from_scheme(Scheme::Forest, Mode::Dark)
                .colors
                .control_active
        );
        // surface is the hex override, not Forest's bg-primary.
        assert_eq!(spec.colors.surface, hexc("#010203"));
    }

    #[test]
    fn overlay_rejects_an_unknown_scheme() {
        let mut spec = ThemeSpec::builtin();
        let file = ThemeFile {
            scheme: Some("chartreuse".into()),
            ..Default::default()
        };
        assert!(spec.overlay(&file).is_err());
    }

    #[test]
    fn resolve_cascades_builtin_then_shared_then_app() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.conf.mix");
        let app = dir.join("app.conf.mix");
        std::fs::write(&shared, "surface: \"#111111\"\nfader_width: 40.0\n").unwrap();
        std::fs::write(&app, "fader_width: 50.0\n").unwrap();

        let spec = resolve_theme(Some(&shared), Some(&app));
        assert_eq!(spec.colors.surface, hexc("#111111"), "shared set surface");
        assert_eq!(spec.metrics.fader_width, 50.0, "app overrides shared");
        assert_eq!(
            spec.colors.panel,
            ThemeSpec::builtin().colors.panel,
            "untouched token stays built-in"
        );

        // A missing file is skipped (not fatal) — built-in remains.
        let only_builtin = resolve_theme(Some(&dir.join("nope.conf.mix")), None);
        assert_eq!(
            only_builtin.colors.surface,
            ThemeSpec::builtin().colors.surface
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typography_fields_cascade_independently_with_provenance() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-typography-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.conf.mix");
        let app = dir.join("app.conf.mix");
        std::fs::write(
            &shared,
            r#"{ typography: { family: "DejaVu Sans", body_px: 14.0 } }"#,
        )
        .unwrap();
        std::fs::write(&app, r#"{ typography: { body_px: 15.5 } }"#).unwrap();

        let spec = resolve_theme(Some(&shared), Some(&app));
        assert_eq!(spec.typography.family, "DejaVu Sans");
        assert_eq!(spec.typography.body_px, 15.5);
        assert_eq!(
            spec.typography_family_provenance,
            TypographyProvenance::SharedTheme
        );
        assert_eq!(
            spec.typography_body_px_provenance,
            TypographyProvenance::AppTheme
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selected_resolve_keeps_selection_and_cascades_file_overrides() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-selected-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("shared.conf.mix");
        let app = dir.join("app.conf.mix");
        std::fs::write(
            &shared,
            "scheme: \"ocean\"\nmode: \"dark\"\nsurface: \"#112233\"\nknob_size: 61\n",
        )
        .unwrap();
        std::fs::write(
            &app,
            "scheme: \"crimson\"\nmode: \"light\"\nsurface: \"#445566\"\n",
        )
        .unwrap();

        let spec =
            resolve_theme_with_selection(Some(&shared), Some(&app), Scheme::Forest, Mode::Dark);
        assert_eq!(spec.scheme, Scheme::Forest);
        assert_eq!(spec.mode, Mode::Dark);
        assert_eq!(spec.colors.surface, hexc("#445566"));
        assert_eq!(spec.metrics.knob_size, 61.0);
        assert_eq!(
            spec.colors.control_active,
            ThemeSpec::from_scheme(Scheme::Forest, Mode::Dark)
                .colors
                .control_active
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selection_write_preserves_overrides_metrics_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-write-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        let original = r##"{
            scheme: "ocean",
            mode: "dark",
            surface: "#010203",
            knob_size: 73,
            future_field: { nested: [1, 2, 3] }
        }"##;
        std::fs::write(&path, original).unwrap();
        let before = cosmix_config::parse_mix_data(original).unwrap();

        file::write_theme_selection(&path, Scheme::Sunset, Mode::Light).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let after = cosmix_config::parse_mix_data(&written).unwrap();
        let (cosmix_config::Value::Map(before), cosmix_config::Value::Map(after)) =
            (&before, &after)
        else {
            panic!("theme is a map");
        };
        assert_eq!(after.get("surface"), before.get("surface"));
        assert_eq!(after.get("knob_size"), before.get("knob_size"));
        assert_eq!(
            after
                .get("future_field")
                .unwrap()
                .to_mix_data_string()
                .unwrap(),
            before
                .get("future_field")
                .unwrap()
                .to_mix_data_string()
                .unwrap()
        );
        assert_eq!(
            after.get("scheme"),
            Some(&cosmix_config::Value::String("sunset".into()))
        );
        assert_eq!(
            after.get("mode"),
            Some(&cosmix_config::Value::String("light".into()))
        );
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files.len(), 2, "target plus persistent advisory lock");
        assert!(files.contains(&"theme.conf.mix".into()));
        assert!(files.contains(&".theme.conf.mix.lock".into()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selection_write_waits_then_preserves_an_interleaved_writers_override() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-serialized-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ scheme: \"ocean\", mode: \"dark\" }").unwrap();

        // Simulate writer A holding the transaction across its read/update.
        let held = file::acquire_theme_write_lock(&path).unwrap();
        let writer_path = path.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(file::write_theme_selection(
                    &writer_path,
                    Scheme::Forest,
                    Mode::Light,
                ))
                .unwrap();
        });
        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "writer B must not read while writer A holds the RMW lock"
        );

        // Writer A commits a new forward-compatible override, then releases.
        crate::fs::write_atomic(
            &path,
            br#"{ scheme: "ocean", mode: "dark", future_override: 42 }"#,
        )
        .unwrap();
        drop(held);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
        writer.join().unwrap();

        let value =
            cosmix_config::parse_mix_data(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let cosmix_config::Value::Map(entries) = &value else {
            panic!("theme is a map");
        };
        assert_eq!(
            entries.get("future_override"),
            Some(&cosmix_config::Value::Number(42.0))
        );
        assert_eq!(
            entries.get("scheme"),
            Some(&cosmix_config::Value::String("forest".into()))
        );
        assert_eq!(
            entries.get("mode"),
            Some(&cosmix_config::Value::String("light".into()))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn contended_theme_write_returns_to_bevy_and_completes_later() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-worker-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ scheme: \"ocean\", mode: \"dark\" }").unwrap();
        let held = file::acquire_theme_write_lock(&path).unwrap();

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        app.world_mut().write_message(ThemeWriteRequest::new(
            path.clone(),
            Scheme::Forest,
            Mode::Light,
        ));

        let before = std::time::Instant::now();
        app.update();
        assert!(
            before.elapsed() < std::time::Duration::from_millis(250),
            "the Bevy update must enqueue rather than wait for the file lock"
        );
        let messages = app
            .world()
            .resource::<bevy::ecs::message::Messages<ThemeWriteCompleted>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);

        drop(held);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let completed = loop {
            app.update();
            let messages = app
                .world()
                .resource::<bevy::ecs::message::Messages<ThemeWriteCompleted>>();
            let mut cursor = messages.get_cursor();
            if let Some(result) = cursor.read(messages).last().cloned() {
                break result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not publish completion"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(completed.path, path);
        assert_eq!(completed.scheme, Scheme::Forest);
        assert_eq!(completed.mode, Mode::Light);
        assert_eq!(completed.result, Ok(()));

        let spec = load_theme_file(&completed.path).unwrap().unwrap();
        assert_eq!(spec.scheme.as_deref(), Some("forest"));
        assert_eq!(spec.mode.as_deref(), Some("light"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn app_exit_drains_an_accepted_theme_write() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-exit-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ scheme: \"ocean\", mode: \"dark\" }").unwrap();
        let held = file::acquire_theme_write_lock(&path).unwrap();

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        app.world_mut().write_message(ThemeWriteRequest::new(
            path.clone(),
            Scheme::Sunset,
            Mode::Light,
        ));
        app.update();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });
        app.world_mut().write_message(AppExit::Success);
        app.update();
        releaser.join().unwrap();

        let spec = load_theme_file(&path).unwrap().unwrap();
        assert_eq!(spec.scheme.as_deref(), Some("sunset"));
        assert_eq!(spec.mode.as_deref(), Some("light"));
        let messages = app
            .world()
            .resource::<bevy::ecs::message::Messages<ThemeWriteCompleted>>();
        let mut cursor = messages.get_cursor();
        let completed = cursor.read(messages).last().unwrap();
        assert_eq!(completed.result, Ok(()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn asynchronous_theme_write_surfaces_validation_failure() {
        let dir =
            std::env::temp_dir().join(format!("ctk-theme-worker-error-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        let malformed = "{ scheme: \"ocean\", surface: \"not-a-colour\" }";
        std::fs::write(&path, malformed).unwrap();

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::default());
        app.world_mut().write_message(ThemeWriteRequest::new(
            path.clone(),
            Scheme::Forest,
            Mode::Light,
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let completed = loop {
            app.update();
            let messages = app
                .world()
                .resource::<bevy::ecs::message::Messages<ThemeWriteCompleted>>();
            let mut cursor = messages.get_cursor();
            if let Some(result) = cursor.read(messages).last().cloned() {
                break result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker did not publish validation failure"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(completed.result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selection_write_leaves_a_malformed_existing_file_untouched() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-invalid-{}", std::process::id()));
        let path = dir.join("theme.conf.mix");
        std::fs::create_dir_all(&dir).unwrap();
        let invalid = "scheme: \"ocean\"\nsurface: \"not-a-colour\"\n";
        std::fs::write(&path, invalid).unwrap();

        assert!(file::write_theme_selection(&path, Scheme::Forest, Mode::Dark).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn focus_gain_reloads_the_app_theme_and_advances_revision() {
        let dir = std::env::temp_dir().join(format!("ctk-theme-focus-{}", std::process::id()));
        let path = dir.join(THEME_FILE);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ scheme: \"forest\", mode: \"light\" }").unwrap();

        let mut app = App::new();
        app.add_plugins(CtkThemePlugin::new(Some(dir.clone())));
        app.world_mut().write_message(WindowFocused {
            window: bevy::prelude::Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();

        let state = app.world().resource::<ThemeState>();
        assert_eq!(state.scheme, Scheme::Forest);
        assert_eq!(state.mode, Mode::Light);
        assert_eq!(state.revision, 1);

        // An unchanged focus reload does not manufacture a restyle revision.
        app.world_mut().write_message(WindowFocused {
            window: bevy::prelude::Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();
        assert_eq!(app.world().resource::<ThemeState>().revision, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
