//! `input.*` wire contract — the headless serde DTOs for `cosmix-inputd`, the
//! AmigaOS `input.device` successor.
//!
//! The AmigaOS inheritance, stated plainly: centralise the input *vocabulary*,
//! not the byte-stream. Keys (and, in evdev, mouse buttons — which ARE keys)
//! become Bus verbs; raw motion stays where it lives; bindings are data,
//! editable by human, agent, or Bus message. This crate is the single source of
//! truth for that vocabulary — Mix scripts, the daemon, and the GUI depend on
//! these DTOs without pulling any toolkit.
//!
//! ## One vocabulary, two layers
//!
//! An action/verb name is a [`cosmix_actions::ActionId`] in BOTH layers — the
//! same identifier an in-app menu binds, a global key fires, and a Bus caller
//! rebinds. There is exactly one binding vocabulary from in-app chords to global
//! keys ([`InputKeymap`]).
//!
//! - **Semantic layer** ([`cosmix_actions::Keymap`]): keysym + side-agnostic
//!   modifiers, layout-surviving, resolved against xkb — the existing
//!   cosmix-actions form, reused verbatim.
//! - **Physical layer** ([`PhysicalBinding`]): an evdev keycode + *side-specific*
//!   modifier state, matched PRE-xkb. This is the ONLY place in the whole stack
//!   where left/right modifiers are distinguished. evdev delivers
//!   `KEY_LEFTCTRL`/`KEY_RIGHTCTRL` as distinct codes; xkb collapses them into
//!   one agnostic `Control`, so no app or ordinary compositor can bind "Right
//!   Ctrl but not Left". A broker reading evdev can — which is exactly why
//!   `RightCtrl+arrows` / `RightShift+arrows` are collision-free global real
//!   estate.
//!
//! ## Invariants encoded here
//!
//! - **No `origin` on a request.** Provenance is broker-stamped from the Bus
//!   `from`; a caller cannot assert it. It appears only on broker-owned records.
//! - **Passthrough is explicit.** A [`PhysicalBinding`] with `passthrough: true`
//!   re-emits the original strokes unchanged (the `RightShift+arrows` default so
//!   Konsole tab-switching survives) while still being a first-class,
//!   claimable row.

use serde::{Deserialize, Serialize};

pub use cosmix_actions::{ActionId, BindingScope, Keymap, RepeatPolicy};

/// Current `keymap.v1` schema version.
pub const KEYMAP_SCHEMA_VERSION: u32 = 1;

/// Side-specific modifier state — the physical layer's reason to exist.
///
/// Unlike [`cosmix_actions`]'s side-agnostic `Modifiers`, each physical modifier
/// key is tracked as its own boolean, so a binding can require exactly Right
/// Control while Left Control passes through untouched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SideModifiers {
    pub left_ctrl: bool,
    pub right_ctrl: bool,
    pub left_shift: bool,
    pub right_shift: bool,
    pub left_alt: bool,
    pub right_alt: bool,
    pub left_super: bool,
    pub right_super: bool,
}

impl SideModifiers {
    /// No modifiers held.
    pub const NONE: Self = Self {
        left_ctrl: false,
        right_ctrl: false,
        left_shift: false,
        right_shift: false,
        left_alt: false,
        right_alt: false,
        left_super: false,
        right_super: false,
    };

    /// Exactly Right Control (the workspace-switch family's left half).
    pub const RIGHT_CTRL: Self = Self {
        right_ctrl: true,
        ..Self::NONE
    };

    /// Exactly Right Shift (the Konsole tab-nav family — passthrough by default).
    pub const RIGHT_SHIFT: Self = Self {
        right_shift: true,
        ..Self::NONE
    };

    /// True when no modifier is held.
    pub const fn is_empty(&self) -> bool {
        !(self.left_ctrl
            || self.right_ctrl
            || self.left_shift
            || self.right_shift
            || self.left_alt
            || self.right_alt
            || self.left_super
            || self.right_super)
    }
}

/// One physical-layer stroke: an evdev keycode plus exact side-specific
/// modifier state, matched before xkb translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PhysicalStroke {
    /// The evdev `KEY_*` code (e.g. `KEY_RIGHT` = 106, `KEY_F5` = 63).
    pub code: u16,
    /// Exact required side-specific modifier state.
    #[serde(default)]
    pub modifiers: SideModifiers,
}

/// A physical-layer binding: a side-specific stroke resolves to a Bus verb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalBinding {
    /// The stroke that fires this row.
    pub stroke: PhysicalStroke,
    /// The Bus verb fired on press — the same id-space as the semantic layer.
    pub action: ActionId,
    /// Focus/global scope, reused from the semantic vocabulary.
    #[serde(default)]
    pub scope: BindingScope,
    /// OS repeat behaviour for the fired verb.
    #[serde(default)]
    pub repeat: RepeatPolicy,
    /// When true, the original strokes are re-emitted unchanged (still a
    /// first-class row that can be claimed per-scope later). The
    /// `RightShift+arrows` default so Konsole tab-switching survives.
    #[serde(default)]
    pub passthrough: bool,
}

/// The full input keymap (`keymap.v1`): the layout-surviving semantic layer plus
/// the side-specific physical layer.
///
/// Not `serde` — the persisted form is a strict-data `.mix` file (the
/// cosmix-actions keymap loader plus the physical rows), owned by
/// `cosmix-input-core`. The Bus wire uses [`BindingRow`] per row, not the whole
/// keymap. (`cosmix_actions::Keymap`/`Binding` are `Serialize`-only by design;
/// they never round-trip through JSON.)
#[derive(Clone, Debug)]
pub struct InputKeymap {
    /// Schema version; [`KEYMAP_SCHEMA_VERSION`].
    pub version: u32,
    /// Keysym rows resolved against xkb (the cosmix-actions form).
    pub semantic: Keymap,
    /// Side-specific pre-xkb rows.
    pub physical: Vec<PhysicalBinding>,
}

impl Default for InputKeymap {
    fn default() -> Self {
        Self {
            version: KEYMAP_SCHEMA_VERSION,
            semantic: Keymap::default(),
            physical: Vec::new(),
        }
    }
}

/// Runtime interception mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    /// Bindings active; matched strokes fire verbs.
    #[default]
    Normal,
    /// The panic hatch — every stroke re-emitted 1:1, no verb fires. Toggled by
    /// `RightCtrl+RightShift+F12`.
    Transparent,
}

/// The `input.*` Bus verb vocabulary served by `inputd`.
pub mod verbs {
    /// Effective binding rows + digest + mode.
    pub const QUERY: &str = "input.query";
    /// Live rebind: validate, mint a generation, apply, write through.
    pub const BIND: &str = "input.bind";
    /// Live unbind.
    pub const UNBIND: &str = "input.unbind";
    /// Transparent/normal toggle.
    pub const MODE: &str = "input.mode";
    /// Re-read the keymap file.
    pub const RELOAD: &str = "input.reload";
    /// Gesture/idle fact subscription.
    pub const OBSERVE: &str = "input.observe";
}

/// The `input` event topics.
pub mod topics {
    /// Key-verb-fired and gesture facts (latest-value, low-rate by construction).
    pub const EVENTS: &str = "input";
    /// Retained digest + rows so any subscriber learns the map on join.
    pub const BINDINGS: &str = "input.bindings";
}

/// A layout-surviving semantic row in wire form — the chord as its textual
/// spelling (`"Ctrl+Shift+T"`, parsed to a [`cosmix_actions::Chord`] by the
/// core). A serde-able mirror of a `cosmix_actions::Binding`, which is itself
/// `Serialize`-only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticRow {
    /// The chord's textual form, e.g. `"Ctrl+Shift+T"` or `"F1"`.
    pub chord: String,
    /// The Bus verb fired.
    pub action: ActionId,
    /// Focus/global scope.
    #[serde(default)]
    pub scope: BindingScope,
    /// OS repeat behaviour.
    #[serde(default)]
    pub repeat: RepeatPolicy,
}

/// The wire form for `input.bind`: either layer, so an agent or human binds a
/// side-specific chord or a layout-surviving keysym through one verb.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "layer", rename_all = "kebab-case")]
pub enum BindingRow {
    /// A side-specific physical row.
    Physical(PhysicalBinding),
    /// A layout-surviving semantic row.
    Semantic(SemanticRow),
}

/// A key-verb-fired fact, published on the [`topics::EVENTS`] topic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyVerbFired {
    /// The verb that fired.
    pub action: ActionId,
    /// The layer that matched.
    pub layer: MatchedLayer,
}

/// Which keymap layer resolved a fired verb.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchedLayer {
    /// The side-specific physical layer matched pre-xkb.
    Physical,
    /// The layout-surviving semantic layer matched.
    Semantic,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_modifiers_distinguish_left_from_right() {
        // The whole reason the physical layer exists: right-only is not left.
        assert_ne!(SideModifiers::RIGHT_CTRL, SideModifiers::NONE);
        assert!(SideModifiers::NONE.is_empty());
        assert!(!SideModifiers::RIGHT_CTRL.is_empty());
        let mut left = SideModifiers::NONE;
        left.left_ctrl = true;
        assert_ne!(left, SideModifiers::RIGHT_CTRL, "L-ctrl must not equal R-ctrl");
    }

    #[test]
    fn physical_binding_round_trips_through_json() {
        // KEY_RIGHT = 106; RightCtrl+Right -> desktop.workspace.next.
        let binding = PhysicalBinding {
            stroke: PhysicalStroke {
                code: 106,
                modifiers: SideModifiers::RIGHT_CTRL,
            },
            action: ActionId::from_static("desktop.workspace.next"),
            scope: BindingScope::default(),
            repeat: RepeatPolicy::default(),
            passthrough: false,
        };
        let json = serde_json::to_string(&binding).expect("serialize");
        let back: PhysicalBinding = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(binding, back);
    }

    #[test]
    fn binding_row_tags_the_layer() {
        let row = BindingRow::Physical(PhysicalBinding {
            stroke: PhysicalStroke {
                code: 63,
                modifiers: SideModifiers::NONE,
            },
            action: ActionId::from_static("user.f05"),
            scope: BindingScope::default(),
            repeat: RepeatPolicy::default(),
            passthrough: false,
        });
        let json = serde_json::to_string(&row).expect("serialize");
        assert!(json.contains("\"layer\":\"physical\""), "tagged: {json}");
        let back: BindingRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(row, back);
    }

    #[test]
    fn empty_keymap_is_version_1() {
        assert_eq!(InputKeymap::default().version, KEYMAP_SCHEMA_VERSION);
    }
}
