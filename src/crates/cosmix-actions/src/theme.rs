//! Canonical desktop theme action ids shared by CTK applications.

use crate::ActionId;

/// Toggle between light and dark mode.
pub const MODE_TOGGLE: ActionId = ActionId::from_static("theme.mode-toggle");
/// Select the Ocean colour scheme.
pub const SCHEME_OCEAN: ActionId = ActionId::from_static("theme.scheme-ocean");
/// Select the Crimson colour scheme.
pub const SCHEME_CRIMSON: ActionId = ActionId::from_static("theme.scheme-crimson");
/// Select the Stone colour scheme.
pub const SCHEME_STONE: ActionId = ActionId::from_static("theme.scheme-stone");
/// Select the Forest colour scheme.
pub const SCHEME_FOREST: ActionId = ActionId::from_static("theme.scheme-forest");
/// Select the Sunset colour scheme.
pub const SCHEME_SUNSET: ActionId = ActionId::from_static("theme.scheme-sunset");
/// Select the Mono colour scheme.
pub const SCHEME_MONO: ActionId = ActionId::from_static("theme.scheme-mono");

/// All shared theme actions in menu order.
pub const ACTION_IDS: [ActionId; 7] = [
    MODE_TOGGLE,
    SCHEME_OCEAN,
    SCHEME_CRIMSON,
    SCHEME_STONE,
    SCHEME_FOREST,
    SCHEME_SUNSET,
    SCHEME_MONO,
];
