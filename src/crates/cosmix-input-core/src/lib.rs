//! Headless deterministic core for `cosmix-inputd`.
//!
//! Everything here is pure: given the keymap, the current mode, and one physical
//! event (keycode + side-specific modifier state + press/repeat/release edge),
//! [`Resolver::resolve`] decides whether a Bus verb fires and whether the
//! original stroke is swallowed. No evdev, no Bus, no clock — the daemon owns
//! all of those and feeds this core the facts.
//!
//! This first cut resolves the **physical layer** (the immediate asks: raw
//! F-keys and the eight `RightCtrl`/`RightShift`+arrow chords). The semantic
//! (xkb keysym) layer is composed in the keymap but resolved in a later pass;
//! the physical layer is what needs the side-specificity the rest of the stack
//! cannot express.
//!
//! ## Release-swallowing (the rule comp's `bindings.rs` already solved)
//!
//! A bound key must swallow BOTH its press and its release. If only the press is
//! swallowed, the downstream app sees a release with no matching press — a
//! stuck-key leak. So [`Resolution::swallow`] is true for every edge of a
//! matched non-passthrough binding, while the verb fires only on the press (and
//! on a repeat when the row's [`RepeatPolicy`] is `Allow`).

use cosmix_input_schema::{
    ActionId, BindingScope, InputKeymap, InputMode, PhysicalBinding, PhysicalStroke, RepeatPolicy,
    SideModifiers,
};

/// A single physical key event's edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The initial key-down.
    Press,
    /// An auto-repeat while held.
    Repeat,
    /// The key-up.
    Release,
}

/// The core's decision for one event.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Resolution {
    /// The Bus verb to fire, if any (only on a firing edge of a bound row).
    pub verb: Option<ActionId>,
    /// The binding's arguments, delivered as the fired verb's body. Present
    /// only when `verb` is (an args-less binding fires with an empty body).
    pub args: Option<serde_json::Value>,
    /// The row's explicit target service. Present only when `verb` is and the
    /// row names one; `None` means "route by the verb's first dot-segment".
    pub service: Option<String>,
    /// When true, do NOT re-emit the original event through uinput.
    pub swallow: bool,
}

impl Resolution {
    /// Re-emit the original event; fire nothing.
    const PASS: Self = Self {
        verb: None,
        args: None,
        service: None,
        swallow: false,
    };
}

/// Why a rebind was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    /// The action id is outside the shared grammar.
    InvalidAction,
    /// `args` must be a JSON object (a verb's body is a map), or absent.
    InvalidArgs,
    /// `args` serialized beyond [`MAX_ARGS_BYTES`].
    ArgsTooLarge,
    /// The keymap already holds this many physical rows (capacity cap).
    AtCapacity,
    /// `service` is not a registered-name-shaped string
    /// (`^[a-z][a-z0-9-]{1,30}$`, the broker's grammar).
    InvalidService,
}

/// True when `name` matches the broker's registered-service grammar,
/// `^[a-z][a-z0-9-]{1,30}$` (noded's `valid_service_name`). A row's explicit
/// `service` target must pass this at bind and at keymap-file load.
pub fn service_is_valid(name: &str) -> bool {
    (2..=31).contains(&name.len())
        && name.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The maximum number of physical rows, a flood/exhaustion backstop.
pub const MAX_PHYSICAL_ROWS: usize = 512;

/// The maximum serialized size of one binding's `args` — the row-count cap
/// bounds rows, this bounds each row's payload (persisted to the keymap file
/// and re-serialized on every fire, auto-repeat included).
pub const MAX_ARGS_BYTES: usize = 4096;

/// The headless resolver: owns the keymap, the mode, and the rebind generation.
#[derive(Clone, Debug)]
pub struct Resolver {
    keymap: InputKeymap,
    mode: InputMode,
    generation: u64,
}

impl Resolver {
    /// Build a resolver over a keymap, starting in [`InputMode::Normal`] at
    /// generation 0.
    pub fn new(keymap: InputKeymap) -> Self {
        Self {
            keymap,
            mode: InputMode::Normal,
            generation: 0,
        }
    }

    /// The current rebind generation — bumped on every applied mutation so a
    /// caller can detect a map it raced.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The current interception mode.
    pub fn mode(&self) -> InputMode {
        self.mode
    }

    /// The live physical rows (for `input.query` / the retained digest).
    pub fn physical_rows(&self) -> &[PhysicalBinding] {
        &self.keymap.physical
    }

    /// Toggle/set the mode. Returns the new generation.
    pub fn set_mode(&mut self, mode: InputMode) -> u64 {
        if self.mode != mode {
            self.mode = mode;
            self.generation += 1;
        }
        self.generation
    }

    /// Resolve one physical event. Deterministic in `(keymap, mode, code, mods,
    /// edge)`.
    pub fn resolve(&self, code: u16, mods: SideModifiers, edge: Edge) -> Resolution {
        // Transparent mode is the panic hatch: every stroke passes 1:1.
        if self.mode == InputMode::Transparent {
            return Resolution::PASS;
        }
        let stroke = PhysicalStroke {
            code,
            modifiers: mods,
        };
        let Some(binding) = self.match_physical(&stroke) else {
            return Resolution::PASS;
        };
        // A declared passthrough row reserves the chord but re-emits it (e.g.
        // RightShift+arrows so Konsole tab-switching survives). No verb, no
        // swallow — but the row still exists so nothing else claims it.
        if binding.passthrough {
            return Resolution::PASS;
        }
        // A bound row swallows every edge (release-swallowing) and fires the
        // verb on the press, and on a repeat only when the policy allows it.
        let verb = match edge {
            Edge::Press => Some(binding.action),
            Edge::Repeat if binding.repeat == RepeatPolicy::Allow => Some(binding.action),
            Edge::Repeat | Edge::Release => None,
        };
        let args = verb.as_ref().and_then(|_| binding.args.clone());
        let service = verb.as_ref().and_then(|_| binding.service.clone());
        Resolution {
            verb,
            args,
            service,
            swallow: true,
        }
    }

    /// Exact physical match: same keycode AND same side-specific modifier state.
    /// Exactness is the point — `RightCtrl+Right` must not match a bare `Right`
    /// or a `LeftCtrl+Right`.
    fn match_physical(&self, stroke: &PhysicalStroke) -> Option<&PhysicalBinding> {
        self.keymap
            .physical
            .iter()
            .find(|binding| binding.stroke == *stroke)
    }

    /// Admit a physical rebind: validate the action grammar, enforce the
    /// capacity cap, replace any row on the same stroke, bump the generation.
    /// The daemon writes the keymap through to disk after this returns Ok.
    pub fn bind_physical(&mut self, binding: PhysicalBinding) -> Result<u64, BindError> {
        if !action_is_valid(&binding.action) {
            return Err(BindError::InvalidAction);
        }
        if binding.service.as_deref().is_some_and(|name| !service_is_valid(name)) {
            return Err(BindError::InvalidService);
        }
        // Args ride as the fired verb's body, and a body is a map: refuse
        // anything but a JSON object so every handler sees a uniform shape,
        // and cap the payload (persisted + re-serialized on every fire).
        if let Some(args) = &binding.args {
            if !args.is_object() {
                return Err(BindError::InvalidArgs);
            }
            let size = serde_json::to_string(args).map(|s| s.len()).unwrap_or(usize::MAX);
            if size > MAX_ARGS_BYTES {
                return Err(BindError::ArgsTooLarge);
            }
        }
        let existing = self
            .keymap
            .physical
            .iter()
            .position(|row| row.stroke == binding.stroke);
        match existing {
            Some(index) => self.keymap.physical[index] = binding,
            None => {
                if self.keymap.physical.len() >= MAX_PHYSICAL_ROWS {
                    return Err(BindError::AtCapacity);
                }
                self.keymap.physical.push(binding);
            }
        }
        self.generation += 1;
        Ok(self.generation)
    }

    /// Replace all physical rows (a keymap file reload). Keeps the current mode;
    /// bumps the generation so callers see the change.
    pub fn replace_physical(&mut self, rows: Vec<PhysicalBinding>) -> u64 {
        self.keymap.physical = rows;
        self.generation += 1;
        self.generation
    }

    /// Remove the physical row on a stroke, if any. Returns the new generation
    /// when a row was removed.
    pub fn unbind_physical(&mut self, stroke: &PhysicalStroke) -> Option<u64> {
        let before = self.keymap.physical.len();
        self.keymap.physical.retain(|row| row.stroke != *stroke);
        if self.keymap.physical.len() != before {
            self.generation += 1;
            Some(self.generation)
        } else {
            None
        }
    }
}

/// The action grammar gate: a Bus verb is a valid [`cosmix_actions::ActionId`].
fn action_is_valid(action: &ActionId) -> bool {
    cosmix_actions::ActionId::validate_str(action.as_str()).is_ok()
}

/// The shipped default keymap: raw F1–F12 as `user.f01`…`user.f12` (no-op until
/// bound), `RightCtrl+←/→` = workspace prev/next, `RightCtrl+↓/↑` = clipboard
/// menu/rotate on the [`CLIPBOARD_SERVICE`] citizen (an explicit `service`
/// target: the citizen is registered as `desktop-vt1` but answers
/// `desktop.clipboard.*`, so first-segment routing cannot reach it), and
/// `RightShift+←/→/↑/↓` as reserved passthrough (Konsole tab nav survives).
/// Evdev codes are the Linux `input-event-codes.h` values.
pub fn default_keymap() -> InputKeymap {
    // input-event-codes.h
    const KEY_LEFT: u16 = 105;
    const KEY_RIGHT: u16 = 106;
    const KEY_UP: u16 = 103;
    const KEY_DOWN: u16 = 108;
    // F1=59..F10=68, F11=87, F12=88.
    const F_CODES: [u16; 12] = [59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88];

    let mut physical = Vec::new();
    for (index, code) in F_CODES.into_iter().enumerate() {
        physical.push(PhysicalBinding {
            stroke: PhysicalStroke {
                code,
                modifiers: SideModifiers::NONE,
            },
            action: user_fkey_action(index + 1),
            args: None,
            service: None,
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Ignore,
            passthrough: false,
        });
    }
    physical.push(right_ctrl_arrow(KEY_LEFT, "desktop.workspace.prev"));
    physical.push(right_ctrl_arrow(KEY_RIGHT, "desktop.workspace.next"));
    // "desktop-vt1" is right on the first host (its user unit runs the citizen
    // `--name desktop-vt1`) and wrong on hosts whose citizen has another name
    // (e.g. one running `desktop-vt5`); it only matters on a freshly seeded file.
    // Deferred: the per-host target is decision 6 in the TODO sweep plan.
    physical.push(clipboard_row(KEY_DOWN, "desktop.clipboard.menu"));
    physical.push(clipboard_row(KEY_UP, "desktop.clipboard.rotate"));
    for code in [KEY_LEFT, KEY_RIGHT, KEY_UP, KEY_DOWN] {
        physical.push(PhysicalBinding {
            stroke: PhysicalStroke {
                code,
                modifiers: SideModifiers::RIGHT_SHIFT,
            },
            // A passthrough row carries an inert marker action; nothing fires it.
            action: ActionId::from_static("input.passthrough"),
            args: None,
            service: None,
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Ignore,
            passthrough: true,
        });
    }
    InputKeymap {
        version: cosmix_input_schema::KEYMAP_SCHEMA_VERSION,
        semantic: cosmix_input_schema::Keymap::default(),
        physical,
    }
}

fn right_ctrl_arrow(code: u16, verb: &'static str) -> PhysicalBinding {
    PhysicalBinding {
        stroke: PhysicalStroke {
            code,
            modifiers: SideModifiers::RIGHT_CTRL,
        },
        action: ActionId::from_static(verb),
        args: None,
        service: None,
        scope: BindingScope::default(),
        // Workspace navigation is incremental — allow auto-repeat.
        repeat: RepeatPolicy::Allow,
        passthrough: false,
    }
}

/// The registered name of the desktop-session clipboard citizen the default
/// `RightCtrl+↓/↑` rows target (`mix --serve desktop-session.mix --name
/// desktop-vt1`).
pub const CLIPBOARD_SERVICE: &str = "desktop-vt1";

fn clipboard_row(code: u16, verb: &'static str) -> PhysicalBinding {
    PhysicalBinding {
        stroke: PhysicalStroke {
            code,
            modifiers: SideModifiers::RIGHT_CTRL,
        },
        action: ActionId::from_static(verb),
        args: None,
        service: Some(CLIPBOARD_SERVICE.to_string()),
        scope: BindingScope::default(),
        // One press, one menu toggle / one rotation — a held key must not
        // flicker the menu or spin the history.
        repeat: RepeatPolicy::Ignore,
        passthrough: false,
    }
}

fn user_fkey_action(n: usize) -> ActionId {
    match n {
        1 => ActionId::from_static("user.f01"),
        2 => ActionId::from_static("user.f02"),
        3 => ActionId::from_static("user.f03"),
        4 => ActionId::from_static("user.f04"),
        5 => ActionId::from_static("user.f05"),
        6 => ActionId::from_static("user.f06"),
        7 => ActionId::from_static("user.f07"),
        8 => ActionId::from_static("user.f08"),
        9 => ActionId::from_static("user.f09"),
        10 => ActionId::from_static("user.f10"),
        11 => ActionId::from_static("user.f11"),
        _ => ActionId::from_static("user.f12"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_RIGHT: u16 = 106;
    const KEY_F5: u16 = 63;

    fn resolver() -> Resolver {
        Resolver::new(default_keymap())
    }

    #[test]
    fn right_ctrl_right_fires_next_workspace_and_swallows() {
        let r = resolver();
        let out = r.resolve(KEY_RIGHT, SideModifiers::RIGHT_CTRL, Edge::Press);
        assert_eq!(out.verb.as_ref().map(|a| a.as_str()), Some("desktop.workspace.next"));
        assert!(out.swallow, "a bound stroke swallows the original");
    }

    #[test]
    fn left_ctrl_right_is_not_the_binding() {
        // The side-specificity that justifies the whole physical layer.
        let r = resolver();
        let mut left = SideModifiers::NONE;
        left.left_ctrl = true;
        let out = r.resolve(KEY_RIGHT, left, Edge::Press);
        assert_eq!(out, Resolution::PASS, "LeftCtrl+Right is not RightCtrl+Right");
    }

    #[test]
    fn bare_right_arrow_passes_through() {
        let r = resolver();
        assert_eq!(
            r.resolve(KEY_RIGHT, SideModifiers::NONE, Edge::Press),
            Resolution::PASS
        );
    }

    #[test]
    fn release_of_a_bound_key_swallows_without_firing() {
        let r = resolver();
        let out = r.resolve(KEY_RIGHT, SideModifiers::RIGHT_CTRL, Edge::Release);
        assert_eq!(out.verb, None, "verbs fire on press, not release");
        assert!(out.swallow, "but the release is still swallowed (no stuck-key leak)");
    }

    #[test]
    fn repeat_honours_the_policy() {
        let r = resolver();
        // Workspace nav allows repeat.
        let nav = r.resolve(KEY_RIGHT, SideModifiers::RIGHT_CTRL, Edge::Repeat);
        assert!(nav.verb.is_some(), "workspace nav repeats");
        // F-keys ignore repeat.
        let fkey = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Repeat);
        assert_eq!(fkey.verb, None, "F-keys do not repeat");
        assert!(fkey.swallow);
    }

    #[test]
    fn right_shift_arrow_is_reserved_passthrough() {
        let r = resolver();
        // Konsole tab nav must survive: declared, but re-emitted, no verb.
        let out = r.resolve(KEY_RIGHT, SideModifiers::RIGHT_SHIFT, Edge::Press);
        assert_eq!(out, Resolution::PASS);
    }

    #[test]
    fn transparent_mode_passes_everything() {
        let mut r = resolver();
        r.set_mode(InputMode::Transparent);
        let out = r.resolve(KEY_RIGHT, SideModifiers::RIGHT_CTRL, Edge::Press);
        assert_eq!(out, Resolution::PASS, "the panic hatch passes even bound strokes");
    }

    #[test]
    fn rebind_f5_and_it_resolves_to_the_new_verb() {
        let mut r = resolver();
        let gen0 = r.generation();
        let g = r
            .bind_physical(PhysicalBinding {
                stroke: PhysicalStroke {
                    code: KEY_F5,
                    modifiers: SideModifiers::NONE,
                },
                action: ActionId::from_static("term.snapshot"),
                args: None,
                service: None,
                scope: BindingScope::default(),
                repeat: RepeatPolicy::Ignore,
                passthrough: false,
            })
            .expect("valid rebind");
        assert!(g > gen0, "a rebind bumps the generation");
        let out = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Press);
        assert_eq!(out.verb.as_ref().map(|a| a.as_str()), Some("term.snapshot"));
    }

    #[test]
    fn bound_args_ride_the_firing_edges_only() {
        let mut r = resolver();
        r.bind_physical(PhysicalBinding {
            stroke: PhysicalStroke {
                code: KEY_F5,
                modifiers: SideModifiers::NONE,
            },
            action: ActionId::from_static("launch.run"),
            args: Some(serde_json::json!({"command": "kcalc"})),
            service: None,
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Allow,
            passthrough: false,
        })
        .expect("valid rebind");
        let press = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Press);
        assert_eq!(press.verb.as_ref().map(|a| a.as_str()), Some("launch.run"));
        assert_eq!(press.args, Some(serde_json::json!({"command": "kcalc"})));
        let repeat = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Repeat);
        assert_eq!(repeat.args, press.args, "an allowed repeat carries the args too");
        let release = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Release);
        assert_eq!(release.verb, None);
        assert_eq!(release.args, None, "no verb, no args");
        assert!(release.swallow);
    }

    #[test]
    fn oversized_args_are_refused() {
        let mut r = resolver();
        let err = r.bind_physical(PhysicalBinding {
            stroke: PhysicalStroke {
                code: KEY_F5,
                modifiers: SideModifiers::NONE,
            },
            action: ActionId::from_static("launch.run"),
            args: Some(serde_json::json!({"command": "x".repeat(MAX_ARGS_BYTES)})),
            service: None,
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Ignore,
            passthrough: false,
        });
        assert_eq!(err, Err(BindError::ArgsTooLarge));
    }

    #[test]
    fn non_object_args_are_refused() {
        let mut r = resolver();
        let err = r.bind_physical(PhysicalBinding {
            stroke: PhysicalStroke {
                code: KEY_F5,
                modifiers: SideModifiers::NONE,
            },
            action: ActionId::from_static("launch.run"),
            args: Some(serde_json::json!("kcalc")),
            service: None,
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Ignore,
            passthrough: false,
        });
        assert_eq!(err, Err(BindError::InvalidArgs), "a body is a map, not a bare value");
    }

    #[test]
    fn unbind_returns_the_stroke_to_passthrough() {
        let mut r = resolver();
        let stroke = PhysicalStroke {
            code: KEY_F5,
            modifiers: SideModifiers::NONE,
        };
        assert!(r.unbind_physical(&stroke).is_some());
        assert_eq!(r.resolve(KEY_F5, SideModifiers::NONE, Edge::Press), Resolution::PASS);
        assert!(r.unbind_physical(&stroke).is_none(), "second unbind is a no-op");
    }

    const KEY_UP: u16 = 103;
    const KEY_DOWN: u16 = 108;

    #[test]
    fn default_clipboard_rows_target_the_citizen_with_the_verb_unchanged() {
        // The live bug: `desktop-vt1.desktop.clipboard.menu` routed to
        // service `desktop-vt1` with the WHOLE string as the command, which the
        // citizen's exact `on desktop.clipboard.menu` never matched.
        let r = resolver();
        for (code, verb) in [
            (KEY_DOWN, "desktop.clipboard.menu"),
            (KEY_UP, "desktop.clipboard.rotate"),
        ] {
            let out = r.resolve(code, SideModifiers::RIGHT_CTRL, Edge::Press);
            assert_eq!(out.verb.as_ref().map(|a| a.as_str()), Some(verb));
            assert_eq!(out.service.as_deref(), Some("desktop-vt1"));
            assert!(out.swallow);
            let repeat = r.resolve(code, SideModifiers::RIGHT_CTRL, Edge::Repeat);
            assert_eq!(repeat.verb, None, "clipboard rows do not auto-repeat");
            assert_eq!(repeat.service, None, "no verb, no target");
        }
    }

    #[test]
    fn a_row_without_service_resolves_no_target() {
        // Absent service = the old first-segment rule; the core reports None.
        let out = resolver().resolve(KEY_RIGHT, SideModifiers::RIGHT_CTRL, Edge::Press);
        assert_eq!(out.service, None);
    }

    fn row_with_service(service: Option<&str>) -> PhysicalBinding {
        PhysicalBinding {
            stroke: PhysicalStroke {
                code: KEY_F5,
                modifiers: SideModifiers::NONE,
            },
            action: ActionId::from_static("desktop.clipboard.menu"),
            args: None,
            service: service.map(str::to_string),
            scope: BindingScope::default(),
            repeat: RepeatPolicy::Ignore,
            passthrough: false,
        }
    }

    #[test]
    fn bind_accepts_a_registered_name_shaped_service() {
        let mut r = resolver();
        for name in ["desktop-vt1", "ab", "a-1", &"a".repeat(31)] {
            r.bind_physical(row_with_service(Some(name)))
                .unwrap_or_else(|e| panic!("{name:?} refused: {e:?}"));
            let out = r.resolve(KEY_F5, SideModifiers::NONE, Edge::Press);
            assert_eq!(out.service.as_deref(), Some(name));
        }
        r.bind_physical(row_with_service(None)).expect("absent service");
    }

    #[test]
    fn bind_refuses_a_malformed_service() {
        let mut r = resolver();
        for name in [
            "",
            "a",
            "Desktop",
            "1desk",
            "-desk",
            "desk.vt1",
            "desk_vt1",
            "desk vt1",
            "desktop-vt1.alpha.bus",
            &"a".repeat(32),
        ] {
            assert_eq!(
                r.bind_physical(row_with_service(Some(name))),
                Err(BindError::InvalidService),
                "{name:?} must be refused"
            );
        }
        assert!(service_is_valid("desktop-vt1"));
    }
}
