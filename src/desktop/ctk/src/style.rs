//! Shared dynamic colour-state helpers for CTK controls.

use bevy::color::Color;
use bevy::feathers::theme::UiTheme;

use crate::theme::{contrast_ratio, ctk_color, tokens, AA_CONTRAST};

pub(crate) const PRESSED_LIFT: f32 = 0.06;
pub(crate) const HOVERED_LIFT: f32 = 0.04;
const RESTING_LIFT: f32 = 0.0;
pub(crate) const DISABLED_LIFT: f32 = 0.03;

#[derive(Clone, Copy, Debug)]
pub(crate) enum InteractionVisualState {
    Pressed,
    Hovered,
    Resting,
    Disabled,
}

impl InteractionVisualState {
    pub(crate) fn legacy_lift(self) -> f32 {
        match self {
            Self::Pressed => PRESSED_LIFT,
            Self::Hovered => HOVERED_LIFT,
            Self::Resting => RESTING_LIFT,
            Self::Disabled => -DISABLED_LIFT,
        }
    }
}

pub(crate) fn selected_background(theme: &UiTheme, state: InteractionVisualState) -> Color {
    selected_background_from_pair(
        ctk_color(theme, &tokens::ROW_SELECTED),
        ctk_color(theme, &tokens::ROW_SELECTED_TEXT),
        ctk_color(theme, &tokens::ROW_SELECTED_TEXT_DIM),
        state,
    )
}

pub(crate) fn selected_background_from_pair(
    base: Color,
    knockout: Color,
    dim: Color,
    state: InteractionVisualState,
) -> Color {
    let magnitude = match state {
        InteractionVisualState::Pressed => PRESSED_LIFT,
        InteractionVisualState::Hovered => HOVERED_LIFT,
        InteractionVisualState::Resting => return base,
        InteractionVisualState::Disabled => DISABLED_LIFT,
    };
    let base_l = bevy::color::Oklcha::from(base).lightness;
    let knockout_l = bevy::color::Oklcha::from(knockout).lightness;
    // Emphasis moves away from the knockout. Disabled deliberately reverses
    // that direction to de-emphasise, then keeps the AA clamp below.
    let away_sign = if base_l >= knockout_l { 1.0 } else { -1.0 };
    let requested_lift = match state {
        InteractionVisualState::Pressed | InteractionVisualState::Hovered => magnitude * away_sign,
        InteractionVisualState::Disabled => -magnitude * away_sign,
        InteractionVisualState::Resting => unreachable!("returned above"),
    };
    contrast_safe_lift(base, requested_lift, &[knockout, dim])
}

/// Preserve as much of a requested selected-row state change as both paired
/// foregrounds can support.
///
/// The built-in knockout pair sits on one side of the bar, so emphasis away
/// from it returns the full requested lift. A legal arbitrary override can
/// straddle the bar; clamping every state prevents a lift chosen from the main
/// knockout from silently breaking the dim foreground on the other side.
pub(crate) fn contrast_safe_lift(base: Color, requested_lift: f32, foregrounds: &[Color]) -> Color {
    const STEPS: u32 = 100;
    (0..=STEPS)
        .rev()
        .find_map(|step| {
            let amount = requested_lift * step as f32 / STEPS as f32;
            let candidate = lighten(base, amount);
            foregrounds
                .iter()
                .all(|foreground| contrast_ratio(*foreground, candidate) >= AA_CONTRAST)
                .then_some(candidate)
        })
        .unwrap_or(base)
}

pub(crate) fn lighten(color: Color, amount: f32) -> Color {
    let c = bevy::color::Oklcha::from(color);
    Color::oklcha(
        (c.lightness + amount).clamp(0.0, 1.0),
        c.chroma,
        c.hue,
        c.alpha,
    )
}
