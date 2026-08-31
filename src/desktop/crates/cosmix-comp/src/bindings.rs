//! Compositor keybindings — the filter that decides whether a key belongs to
//! the compositor or to the focused client.
//!
//! This module is deliberately pure: it owns no Wayland state and performs no
//! I/O, so the matching rules are unit-testable and the protocol thread only
//! ever does table lookups and set bookkeeping here. Anything an intercepted
//! binding *causes* is decided by the caller, not by this module — see
//! [`BindingAction`] for which thread owns each action.
//!
//! Design consult: codex, 2026-07-31, verified against smithay 0.7.0 and
//! xkbcommon 0.8.0 sources rather than from memory.
//!
//! # Why keysyms and not keycodes
//!
//! A binding is written against what the user *typed*, so it must survive
//! layout changes; matching raw evdev codes would move `Super+Q` to a
//! different physical key on AZERTY. Raw keycodes appear here for exactly one
//! purpose: remembering which presses were swallowed so the matching release
//! can be swallowed too.
//!
//! # The release-swallowing rule
//!
//! Modifiers can change between a press and its release. Releasing `Super`
//! before `Q` in `Super+Q` means the `Q` release arrives with no modifiers and
//! would not match the binding — forwarding it hands the client a release for
//! a press it never saw, which terminals interpret as a stuck key. So a
//! release is swallowed on the strength of the *press* having been
//! intercepted, never by re-matching the binding. The same reasoning is why
//! disabling interception mid-chord still swallows already-pending releases.

use std::collections::HashSet;

use smithay::input::keyboard::{Keycode, Keysym, ModifiersState, keysyms};

/// The non-lock modifiers a binding can require or forbid.
///
/// Caps Lock and Num Lock are deliberately absent: they are toggles a user
/// leaves on for unrelated reasons, and a compositor binding that stops
/// working because Num Lock is on is a bug report nobody diagnoses. They are
/// governed by [`ModifierPattern::ignore_locks`] instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifierSet {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub iso_level3_shift: bool,
    pub iso_level5_shift: bool,
}

impl ModifierSet {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
        iso_level3_shift: false,
        iso_level5_shift: false,
    };

    pub const fn logo() -> Self {
        Self {
            logo: true,
            ..Self::NONE
        }
    }

    pub const fn with_ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }

    pub const fn with_alt(mut self) -> Self {
        self.alt = true;
        self
    }

    pub const fn with_shift(mut self) -> Self {
        self.shift = true;
        self
    }

    /// The modifiers currently active, as reported by xkb's effective state.
    ///
    /// Effective state — not `depressed` — is what makes latched modifiers and
    /// sticky-keys accessibility settings work: a latched Super is not
    /// physically held but is semantically active.
    fn from_state(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
            iso_level3_shift: state.iso_level3_shift,
            iso_level5_shift: state.iso_level5_shift,
        }
    }

    /// The modifiers this set does *not* contain.
    const fn complement(self) -> Self {
        Self {
            ctrl: !self.ctrl,
            alt: !self.alt,
            shift: !self.shift,
            logo: !self.logo,
            iso_level3_shift: !self.iso_level3_shift,
            iso_level5_shift: !self.iso_level5_shift,
        }
    }

    /// Modifier names in a stable order, for the structured listing.
    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.ctrl {
            names.push("ctrl");
        }
        if self.alt {
            names.push("alt");
        }
        if self.shift {
            names.push("shift");
        }
        if self.logo {
            names.push("logo");
        }
        if self.iso_level3_shift {
            names.push("iso_level3_shift");
        }
        if self.iso_level5_shift {
            names.push("iso_level5_shift");
        }
        names
    }
}

/// Which modifiers must be held, and which must not.
///
/// Stating the forbidden set explicitly rather than deriving "everything else"
/// at match time is what lets a future config express a deliberately loose
/// binding (`Super+Q`, don't care about Shift) without changing the matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModifierPattern {
    pub required: ModifierSet,
    pub forbidden: ModifierSet,
    /// When true (the default), Caps Lock and Num Lock are not consulted.
    pub ignore_locks: bool,
}

impl ModifierPattern {
    /// Exactly these modifiers and no others — the safe default for a
    /// compositor binding, because a loose pattern silently steals every
    /// superset chord from the focused client.
    pub const fn exact(required: ModifierSet) -> Self {
        Self {
            required,
            forbidden: required.complement(),
            ignore_locks: true,
        }
    }

    fn matches(&self, state: &ModifiersState) -> bool {
        let active = ModifierSet::from_state(state);
        let required_ok = (!self.required.ctrl || active.ctrl)
            && (!self.required.alt || active.alt)
            && (!self.required.shift || active.shift)
            && (!self.required.logo || active.logo)
            && (!self.required.iso_level3_shift || active.iso_level3_shift)
            && (!self.required.iso_level5_shift || active.iso_level5_shift);
        let forbidden_ok = (!self.forbidden.ctrl || !active.ctrl)
            && (!self.forbidden.alt || !active.alt)
            && (!self.forbidden.shift || !active.shift)
            && (!self.forbidden.logo || !active.logo)
            && (!self.forbidden.iso_level3_shift || !active.iso_level3_shift)
            && (!self.forbidden.iso_level5_shift || !active.iso_level5_shift);
        let locks_ok = self.ignore_locks || (!state.caps_lock && !state.num_lock);
        required_ok && forbidden_ok && locks_ok
    }
}

/// Every Phase 1 binding fires on key press. The structured listing still
/// reports a `trigger` field so that adding release-triggered bindings later
/// (tap-Super-for-overview is the obvious one) is a schema addition rather
/// than a schema change — but the enum itself waits until something
/// constructs it.
const PHASE1_TRIGGER: &str = "press";

/// What an intercepted binding does.
///
/// The variant determines which thread executes it, and that split is the
/// load-bearing part: the filter closure runs on the protocol thread, where
/// blocking would stall every client.
///
/// * [`RequestCloseFocused`](Self::RequestCloseFocused) is a pure protocol
///   operation — it queues an `xdg_toplevel.close` event and returns — so it
///   runs inline on the protocol thread.
/// * [`RestoreMostRecentlyMinimized`](Self::RestoreMostRecentlyMinimized)
///   mutates compositor scene policy and focus on the protocol thread.
/// * [`ExitNestedCompositor`](Self::ExitNestedCompositor) is ECS lifecycle and
///   is sent over a bounded channel for a Bevy system to execute.
/// * [`ToggleInterception`](Self::ToggleInterception) mutates only this
///   module's state, so it too runs inline.
/// * [`SwitchVt`](Self::SwitchVt) publishes a coordinator request and returns;
///   only the session thread may call libseat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingAction {
    /// Send `xdg_toplevel.close` to the focused toplevel. Deliberately not a
    /// client kill: the client is entitled to ignore it and prompt the user.
    RequestCloseFocused,
    /// Restore the most recently minimised live toplevel.
    RestoreMostRecentlyMinimized,
    /// Ask the Bevy app to exit. Nested-session lifecycle only — bare-metal
    /// Phase 2 will not carry this binding.
    ExitNestedCompositor,
    /// Turn normal interception off or back on. Always reserved.
    ToggleInterception,
    /// Ask the live KMS coordinator to switch to one Linux VT.
    SwitchVt(u8),
}

impl BindingAction {
    pub const fn name(self) -> &'static str {
        match self {
            Self::RequestCloseFocused => "RequestCloseFocused",
            Self::RestoreMostRecentlyMinimized => "RestoreMostRecentlyMinimized",
            Self::ExitNestedCompositor => "ExitNestedCompositor",
            Self::ToggleInterception => "ToggleInterception",
            Self::SwitchVt(_) => "SwitchVt",
        }
    }

    /// Whether the action must leave the protocol thread to be executed.
    pub const fn needs_ecs(self) -> bool {
        matches!(self, Self::ExitNestedCompositor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BindingProfile {
    Nested,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    KmsLive,
}

/// One entry of the binding table.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// Stable identifier. Agents and config match on this, never on position.
    pub id: &'static str,
    /// The keysym as it appears on a US layout, matched via
    /// `raw_latin_sym_or_raw_current_sym`. This deliberately follows the
    /// mnemonic logical symbol across layouts rather than a physical key.
    pub keysym: u32,
    /// Human-readable keysym name for the structured listing.
    pub keysym_name: &'static str,
    pub modifiers: ModifierPattern,
    pub action: BindingAction,
    /// The escape-hatch binding. Reserved bindings remain matchable while
    /// normal interception is disabled, so the escape hatch can always be
    /// re-armed without restarting the compositor.
    pub reserved: bool,
}

/// The compiled binding table.
#[derive(Clone, Debug)]
pub struct BindingTable {
    bindings: Vec<Binding>,
}

impl BindingTable {
    /// The Phase 1 set.
    ///
    /// `Super+Q` proves focus resolution plus a
    /// protocol-thread action; `Super+Shift+Escape` proves the whole
    /// filter → bounded channel → Bevy path; the toggle proves the escape
    /// hatch. Spawn-a-terminal is *not* here: it would prove no new keyboard
    /// mechanism while dragging in launcher policy, environment propagation
    /// and child-process lifecycle.
    pub fn phase1_defaults() -> Self {
        Self {
            bindings: vec![
                Binding {
                    id: "close-focused",
                    keysym: keysyms::KEY_q,
                    keysym_name: "q",
                    modifiers: ModifierPattern::exact(ModifierSet::logo()),
                    action: BindingAction::RequestCloseFocused,
                    reserved: false,
                },
                restore_minimized_binding(),
                Binding {
                    id: "exit-nested-compositor",
                    keysym: keysyms::KEY_Escape,
                    keysym_name: "Escape",
                    modifiers: ModifierPattern::exact(ModifierSet::logo().with_shift()),
                    action: BindingAction::ExitNestedCompositor,
                    reserved: false,
                },
                Binding {
                    id: "toggle-interception",
                    keysym: keysyms::KEY_F12,
                    keysym_name: "F12",
                    modifiers: ModifierPattern::exact(ModifierSet::logo().with_ctrl().with_shift()),
                    action: BindingAction::ToggleInterception,
                    reserved: true,
                },
            ],
        }
    }

    pub fn kms_live_defaults() -> Self {
        const VT_KEYS: [(&str, u32, &str, u8); 12] = [
            ("switch-vt-1", keysyms::KEY_F1, "F1", 1),
            ("switch-vt-2", keysyms::KEY_F2, "F2", 2),
            ("switch-vt-3", keysyms::KEY_F3, "F3", 3),
            ("switch-vt-4", keysyms::KEY_F4, "F4", 4),
            ("switch-vt-5", keysyms::KEY_F5, "F5", 5),
            ("switch-vt-6", keysyms::KEY_F6, "F6", 6),
            ("switch-vt-7", keysyms::KEY_F7, "F7", 7),
            ("switch-vt-8", keysyms::KEY_F8, "F8", 8),
            ("switch-vt-9", keysyms::KEY_F9, "F9", 9),
            ("switch-vt-10", keysyms::KEY_F10, "F10", 10),
            ("switch-vt-11", keysyms::KEY_F11, "F11", 11),
            ("switch-vt-12", keysyms::KEY_F12, "F12", 12),
        ];
        let modifiers = ModifierPattern::exact(ModifierSet::NONE.with_ctrl().with_alt());
        let bindings = VT_KEYS
            .into_iter()
            .map(|(id, keysym, keysym_name, vt)| Binding {
                id,
                keysym,
                keysym_name,
                modifiers,
                action: BindingAction::SwitchVt(vt),
                reserved: true,
            })
            .chain(std::iter::once(restore_minimized_binding()))
            .collect();
        Self { bindings }
    }

    fn find(
        &self,
        keysym: u32,
        state: &ModifiersState,
        normal_interception_enabled: bool,
    ) -> Option<&Binding> {
        self.bindings.iter().find(|binding| {
            (normal_interception_enabled || binding.reserved)
                && binding.keysym == keysym
                && binding.modifiers.matches(state)
        })
    }
}

const fn restore_minimized_binding() -> Binding {
    Binding {
        id: "restore-recent-minimized",
        keysym: keysyms::KEY_m,
        keysym_name: "m",
        modifiers: ModifierPattern::exact(ModifierSet::logo().with_shift()),
        action: BindingAction::RestoreMostRecentlyMinimized,
        reserved: false,
    }
}

/// What the filter closure should do with a key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyDisposition {
    /// Not ours — the client sees it.
    Forward,
    /// Ours, and it means something.
    Act(BindingAction),
    /// Ours only because we swallowed the matching press. No action.
    SwallowRelease,
}

/// Protocol-thread-local keybinding state.
///
/// Owned by the Wayland state struct so it needs no lock: the only thread that
/// can reach it is the one already serialising all protocol work.
#[derive(Debug)]
pub struct BindingState {
    table: BindingTable,
    enabled: bool,
    /// Raw keycodes whose press we intercepted, awaiting their release.
    ///
    /// Wayland client focus changes deliberately do not clear this set: a new
    /// client must not receive a release for a press it never received. Host
    /// window focus loss is authoritative instead; the protocol thread feeds
    /// releases for every Smithay-pressed key through this same dispatch path,
    /// reconciling the intercepted and forwarded sets together.
    intercepted: HashSet<u32>,
}

impl BindingState {
    pub fn new(table: BindingTable, enabled: bool) -> Self {
        Self {
            table,
            enabled,
            intercepted: HashSet::new(),
        }
    }

    pub fn phase1(enabled: bool) -> Self {
        Self::new(BindingTable::phase1_defaults(), enabled)
    }

    pub(crate) fn for_profile(profile: BindingProfile, enabled: bool) -> Self {
        match profile {
            BindingProfile::Nested => Self::phase1(enabled),
            BindingProfile::KmsLive => Self::new(BindingTable::kms_live_defaults(), enabled),
        }
    }

    /// Flip interception. Pending releases are *not* cleared — see the module
    /// docs on the release-swallowing rule.
    pub fn toggle_interception(&mut self) -> bool {
        self.enabled = !self.enabled;
        self.enabled
    }

    /// Decide a key event. `keysym` is the Latin-fallback symbol; `None` means
    /// the key produced no usable symbol, which can never match a binding.
    pub fn dispatch(
        &mut self,
        keycode: Keycode,
        pressed: bool,
        keysym: Option<Keysym>,
        modifiers: &ModifiersState,
    ) -> KeyDisposition {
        let raw_code = keycode.raw();

        if !pressed {
            // Release path. Swallow if and only if we swallowed the press,
            // regardless of what the modifiers look like now, and regardless
            // of whether interception has since been turned off.
            return if self.intercepted.remove(&raw_code) {
                KeyDisposition::SwallowRelease
            } else {
                KeyDisposition::Forward
            };
        }

        let Some(keysym) = keysym else {
            return KeyDisposition::Forward;
        };
        let Some(binding) = self.table.find(keysym.raw(), modifiers, self.enabled) else {
            return KeyDisposition::Forward;
        };

        let action = binding.action;
        self.intercepted.insert(raw_code);
        KeyDisposition::Act(action)
    }

    /// Decide a key while ext-session-lock owns the seat. Only the KMS VT
    /// escape route remains a compositor binding; every ordinary chord is
    /// forwarded to the lock surface and never enters intercepted bookkeeping.
    pub(crate) fn dispatch_session_locked(
        &mut self,
        keycode: Keycode,
        pressed: bool,
        keysym: Option<Keysym>,
        modifiers: &ModifiersState,
    ) -> KeyDisposition {
        let raw_code = keycode.raw();
        if !pressed {
            return if self.intercepted.remove(&raw_code) {
                KeyDisposition::SwallowRelease
            } else {
                KeyDisposition::Forward
            };
        }
        let Some(keysym) = keysym else {
            return KeyDisposition::Forward;
        };
        let Some(binding) = self.table.find(keysym.raw(), modifiers, self.enabled) else {
            return KeyDisposition::Forward;
        };
        if !matches!(binding.action, BindingAction::SwitchVt(_)) {
            return KeyDisposition::Forward;
        }
        self.intercepted.insert(raw_code);
        KeyDisposition::Act(binding.action)
    }

    /// The binding table as Mix strict-data.
    ///
    /// Strict-data rather than JSON because it is the substrate's format for
    /// machine-readable state, and `data_parse` will not execute it. Every
    /// value emitted here is drawn from a fixed ASCII vocabulary — binding
    /// ids, keysym names, modifier names, action names — so no escaping is
    /// required; if that ever stops being true this must gain an escaper.
    pub fn to_strict_data(&self) -> String {
        let mut out = String::from("{\n");
        out.push_str("  \"schema_version\": 1,\n");
        out.push_str(&format!("  \"interception_enabled\": {},\n", self.enabled));
        out.push_str("  \"bindings\": [\n");
        for (index, binding) in self.table.bindings.iter().enumerate() {
            let comma = if index + 1 == self.table.bindings.len() {
                ""
            } else {
                ","
            };
            out.push_str("    {\n");
            out.push_str(&format!("      \"id\": \"{}\",\n", binding.id));
            out.push_str(&format!("      \"keysym\": \"{}\",\n", binding.keysym_name));
            out.push_str(&format!("      \"keysym_value\": {},\n", binding.keysym));
            out.push_str(&format!(
                "      \"required\": [{}],\n",
                quoted_list(&binding.modifiers.required.names())
            ));
            out.push_str(&format!(
                "      \"forbidden\": [{}],\n",
                quoted_list(&binding.modifiers.forbidden.names())
            ));
            out.push_str(&format!(
                "      \"ignore_locks\": {},\n",
                binding.modifiers.ignore_locks
            ));
            out.push_str(&format!("      \"trigger\": \"{PHASE1_TRIGGER}\",\n"));
            out.push_str(&format!(
                "      \"action\": \"{}\",\n",
                binding.action.name()
            ));
            out.push_str(&format!("      \"reserved\": {}\n", binding.reserved));
            out.push_str(&format!("    }}{comma}\n"));
        }
        out.push_str("  ]\n");
        out.push_str("}\n");
        out
    }
}

fn quoted_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|item| format!("\"{item}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(logo: bool, shift: bool, ctrl: bool) -> ModifiersState {
        ModifiersState {
            logo,
            shift,
            ctrl,
            ..ModifiersState::default()
        }
    }

    fn code(raw: u32) -> Keycode {
        Keycode::new(raw)
    }

    fn sym(raw: u32) -> Option<Keysym> {
        Some(Keysym::new(raw))
    }

    #[test]
    fn super_q_is_intercepted_and_its_release_swallowed() {
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(
                code(24),
                true,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::Act(BindingAction::RequestCloseFocused)
        );
        assert_eq!(state.intercepted.len(), 1);
        assert_eq!(
            state.dispatch(
                code(24),
                false,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::SwallowRelease
        );
        assert_eq!(state.intercepted.len(), 0);
    }

    #[test]
    fn release_is_swallowed_even_after_the_modifier_is_let_go_first() {
        // The regression this whole design exists for: Super released before Q
        // means the Q release arrives bare and would not re-match the binding.
        let mut state = BindingState::phase1(true);
        state.dispatch(
            code(24),
            true,
            sym(keysyms::KEY_q),
            &mods(true, false, false),
        );
        assert_eq!(
            state.dispatch(
                code(24),
                false,
                sym(keysyms::KEY_q),
                &mods(false, false, false)
            ),
            KeyDisposition::SwallowRelease
        );
    }

    #[test]
    fn a_release_we_never_swallowed_is_forwarded() {
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(
                code(24),
                false,
                sym(keysyms::KEY_q),
                &mods(false, false, false)
            ),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn plain_q_reaches_the_client() {
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(
                code(24),
                true,
                sym(keysyms::KEY_q),
                &mods(false, false, false)
            ),
            KeyDisposition::Forward
        );
        assert_eq!(state.intercepted.len(), 0);
    }

    #[test]
    fn a_superset_chord_does_not_match_an_exact_binding() {
        // Ctrl+Super+Q belongs to the client, not to us.
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(
                code(24),
                true,
                sym(keysyms::KEY_q),
                &mods(true, false, true)
            ),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn lock_modifiers_do_not_break_a_binding() {
        let mut state = BindingState::phase1(true);
        let with_locks = ModifiersState {
            logo: true,
            caps_lock: true,
            num_lock: true,
            ..Default::default()
        };
        assert_eq!(
            state.dispatch(code(24), true, sym(keysyms::KEY_q), &with_locks),
            KeyDisposition::Act(BindingAction::RequestCloseFocused)
        );
    }

    #[test]
    fn locks_are_honoured_when_the_pattern_asks() {
        let mut pattern = ModifierPattern::exact(ModifierSet::logo());
        pattern.ignore_locks = false;
        let with_caps = ModifiersState {
            logo: true,
            caps_lock: true,
            ..Default::default()
        };
        assert!(!pattern.matches(&with_caps));
        assert!(pattern.matches(&ModifiersState {
            logo: true,
            ..Default::default()
        }));
    }

    #[test]
    fn exit_binding_requires_both_modifiers() {
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(
                code(9),
                true,
                sym(keysyms::KEY_Escape),
                &mods(true, false, false)
            ),
            KeyDisposition::Forward
        );
        assert_eq!(
            state.dispatch(
                code(9),
                true,
                sym(keysyms::KEY_Escape),
                &mods(true, true, false)
            ),
            KeyDisposition::Act(BindingAction::ExitNestedCompositor)
        );
    }

    #[test]
    fn disabled_interception_forwards_non_reserved_bindings_but_matches_the_reserved_toggle() {
        let mut state = BindingState::phase1(false);
        assert_eq!(
            state.dispatch(
                code(24),
                true,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::Forward
        );
        let toggle = ModifiersState {
            logo: true,
            shift: true,
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            state.dispatch(code(96), true, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::Act(BindingAction::ToggleInterception)
        );
    }

    #[test]
    fn toggling_off_mid_chord_still_swallows_the_pending_release() {
        let mut state = BindingState::phase1(true);
        state.dispatch(
            code(24),
            true,
            sym(keysyms::KEY_q),
            &mods(true, false, false),
        );
        assert!(!state.toggle_interception());
        assert_eq!(
            state.dispatch(
                code(24),
                false,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::SwallowRelease
        );
    }

    #[test]
    fn reserved_toggle_chord_round_trips_through_dispatch() {
        let mut state = BindingState::phase1(true);
        let toggle = mods(true, true, true);

        assert_eq!(
            state.dispatch(code(96), true, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::Act(BindingAction::ToggleInterception)
        );
        assert!(!state.toggle_interception());
        assert_eq!(
            state.dispatch(code(96), false, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::SwallowRelease
        );

        assert_eq!(
            state.dispatch(code(96), true, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::Act(BindingAction::ToggleInterception)
        );
        assert!(state.toggle_interception());
        assert_eq!(
            state.dispatch(code(96), false, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::SwallowRelease
        );
    }

    #[test]
    fn forwarded_press_stays_forwarded_if_interception_is_reenabled_before_release() {
        let mut state = BindingState::phase1(false);
        assert_eq!(
            state.dispatch(
                code(24),
                true,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::Forward
        );

        let toggle = mods(true, true, true);
        assert_eq!(
            state.dispatch(code(96), true, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::Act(BindingAction::ToggleInterception)
        );
        assert!(state.toggle_interception());
        assert_eq!(
            state.dispatch(code(96), false, sym(keysyms::KEY_F12), &toggle),
            KeyDisposition::SwallowRelease
        );

        assert_eq!(
            state.dispatch(
                code(24),
                false,
                sym(keysyms::KEY_q),
                &mods(true, false, false)
            ),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn a_key_with_no_symbol_is_forwarded() {
        let mut state = BindingState::phase1(true);
        assert_eq!(
            state.dispatch(code(24), true, None, &mods(true, false, false)),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn every_action_declares_where_it_runs() {
        assert!(BindingAction::ExitNestedCompositor.needs_ecs());
        assert!(!BindingAction::RequestCloseFocused.needs_ecs());
        assert!(!BindingAction::RestoreMostRecentlyMinimized.needs_ecs());
        assert!(!BindingAction::ToggleInterception.needs_ecs());
        assert!(!BindingAction::SwitchVt(1).needs_ecs());
    }

    #[test]
    fn nested_profile_never_intercepts_ctrl_alt_function_keys() {
        let mut state = BindingState::for_profile(BindingProfile::Nested, true);
        let modifiers = ModifiersState {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        for (index, keysym) in [
            keysyms::KEY_F1,
            keysyms::KEY_F2,
            keysyms::KEY_F3,
            keysyms::KEY_F4,
            keysyms::KEY_F5,
            keysyms::KEY_F6,
            keysyms::KEY_F7,
            keysyms::KEY_F8,
            keysyms::KEY_F9,
            keysyms::KEY_F10,
            keysyms::KEY_F11,
            keysyms::KEY_F12,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                state.dispatch(code(67 + index as u32), true, sym(keysym), &modifiers),
                KeyDisposition::Forward
            );
        }
    }

    #[test]
    fn kms_live_profile_maps_exact_ctrl_alt_function_keys_to_vts() {
        let mut state = BindingState::for_profile(BindingProfile::KmsLive, true);
        let modifiers = ModifiersState {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        for (index, keysym) in [
            keysyms::KEY_F1,
            keysyms::KEY_F2,
            keysyms::KEY_F3,
            keysyms::KEY_F4,
            keysyms::KEY_F5,
            keysyms::KEY_F6,
            keysyms::KEY_F7,
            keysyms::KEY_F8,
            keysyms::KEY_F9,
            keysyms::KEY_F10,
            keysyms::KEY_F11,
            keysyms::KEY_F12,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                state.dispatch(code(67 + index as u32), true, sym(keysym), &modifiers),
                KeyDisposition::Act(BindingAction::SwitchVt(index as u8 + 1))
            );
            assert_eq!(
                state.dispatch(code(67 + index as u32), false, sym(keysym), &modifiers),
                KeyDisposition::SwallowRelease
            );
        }

        let with_shift = ModifiersState {
            ctrl: true,
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            state.dispatch(code(67), true, sym(keysyms::KEY_F1), &with_shift),
            KeyDisposition::Forward
        );
    }

    #[test]
    fn the_default_table_has_exactly_one_reserved_binding() {
        // Structured consumers can identify the only binding that remains
        // active while normal interception is disabled.
        let table = BindingTable::phase1_defaults();
        assert_eq!(
            table
                .bindings
                .iter()
                .filter(|binding| binding.reserved)
                .count(),
            1
        );
        assert_eq!(table.bindings.len(), 4);
    }

    #[test]
    fn both_profiles_restore_the_recent_minimized_window_with_super_shift_m() {
        let modifiers = ModifiersState {
            logo: true,
            shift: true,
            ..Default::default()
        };
        for profile in [BindingProfile::Nested, BindingProfile::KmsLive] {
            let mut state = BindingState::for_profile(profile, true);
            assert!(
                state
                    .to_strict_data()
                    .contains("\"id\": \"restore-recent-minimized\"")
            );
            assert_eq!(
                state.dispatch(code(50), true, sym(keysyms::KEY_m), &modifiers),
                KeyDisposition::Act(BindingAction::RestoreMostRecentlyMinimized)
            );
            assert_eq!(
                state.dispatch(code(50), false, sym(keysyms::KEY_m), &modifiers),
                KeyDisposition::SwallowRelease
            );
        }
    }

    #[test]
    fn binding_ids_are_unique() {
        let table = BindingTable::phase1_defaults();
        let mut ids: Vec<_> = table.bindings.iter().map(|binding| binding.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn strict_data_listing_reports_state_and_every_binding() {
        let state = BindingState::phase1(true);
        let listing = state.to_strict_data();
        assert!(listing.contains("\"schema_version\": 1"));
        assert!(listing.contains("\"interception_enabled\": true"));
        for binding in &BindingTable::phase1_defaults().bindings {
            assert!(
                listing.contains(binding.id),
                "{} missing from listing",
                binding.id
            );
            assert!(listing.contains(binding.action.name()));
        }
        assert!(listing.contains("\"required\": [\"logo\"]"));
        // The listing must be free of the strict-data lexer's live characters,
        // since it is emitted without an escaper.
        assert!(!listing.contains('$'));
        assert!(!listing.contains('\\'));
    }

    #[test]
    fn disabled_listing_says_so() {
        assert!(
            BindingState::phase1(false)
                .to_strict_data()
                .contains("\"interception_enabled\": false")
        );
    }
}
