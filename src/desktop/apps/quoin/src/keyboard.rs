//! Configured keyboard bindings (shell doc §5, §8.4): per-edge pin, dock and
//! hide, and the one "cycle focus through shell panels" binding, all read
//! from `conf.mix` ([`ShellConfig`]) and unbound by default.
//!
//! Scope: the layer host has no global key grab. Keys reach Quoin only while
//! one of its own panel surfaces holds the keyboard, so a binding can never
//! shadow an application's shortcut, and a key Quoin received proves the
//! focused window is its panel on this output. Reaching a binding while an
//! application is focused needs a compositor-side chord grab; that route is
//! not built here, and when it is, its target comes from the same
//! `cosmix_shell::core::keyboard_target_output` rule, fed by comp's focus and
//! pointer reports.
//!
//! Mode bindings dispatch through the precise mode verb, never the legacy
//! pin/unpin pair, so a keyboard pin is exactly a menu or Bus pin.

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_shell::core::{Edge, PanelMode};
use cosmix_shell::runtime::{
    ShellCommand, ShellFrameState, ShellRuntimeSet, ShellSemanticVerb, ShellStagedIngress,
    focus_next_command, semantic_shell_command,
};

use crate::config::{ConfigIngest, ShellConfig};

/// What one configured chord does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAction {
    Mode(Edge, PanelMode),
    CycleFocus,
}

/// The modifiers a binding chord can name, in `conf.mix`'s canonical order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
}

/// Modifier keys held, as the key stream itself reports them in event order —
/// so a chord is judged by the modifiers down at that press, not by the state
/// at the end of the frame. Either side counts, except that Right Alt is Alt
/// only when the layout says so: as AltGr (logical `AltGraph`) it selects a
/// third-level character, so AltGr+Q typing `@` must not be `Alt+Q`.
#[derive(Default)]
struct HeldModifiers(Vec<KeyCode>);

impl HeldModifiers {
    const KEYS: [KeyCode; 8] = [
        KeyCode::ControlLeft,
        KeyCode::ControlRight,
        KeyCode::AltLeft,
        KeyCode::AltRight,
        KeyCode::ShiftLeft,
        KeyCode::ShiftRight,
        KeyCode::SuperLeft,
        KeyCode::SuperRight,
    ];

    /// Track one event; true when it was a modifier key.
    fn observe(&mut self, input: &KeyboardInput) -> bool {
        if !Self::KEYS.contains(&input.key_code) {
            return false;
        }
        self.0.retain(|&key| key != input.key_code);
        let altgr = input.key_code == KeyCode::AltRight && input.logical_key != Key::Alt;
        if input.state == ButtonState::Pressed && !altgr {
            self.0.push(input.key_code);
        }
        true
    }

    fn modifiers(&self) -> Modifiers {
        let any = |pair: [KeyCode; 2]| self.0.iter().any(|key| pair.contains(key));
        Modifiers {
            ctrl: any([KeyCode::ControlLeft, KeyCode::ControlRight]),
            alt: any([KeyCode::AltLeft, KeyCode::AltRight]),
            shift: any([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
            super_key: any([KeyCode::SuperLeft, KeyCode::SuperRight]),
        }
    }
}

/// Named keys by their logical value, so a keypad arrow or keypad Enter
/// (NumLock off) matches `Left` or `Return` like the main-block key.
fn logical_name(logical: &Key) -> Option<&'static str> {
    Some(match logical {
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::Tab => "Tab",
        Key::Enter => "Return",
        Key::Space => "space",
        Key::Escape => "Escape",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "Page_Up",
        Key::PageDown => "Page_Down",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Backspace => "BackSpace",
        _ => return None,
    })
}

/// A binding's key name in the config grammar. Letters and digits follow the
/// layout (the logical key, as comp's own bindings do), so `Super+Shift+D` is
/// the key labelled D; named keys use their logical value. Otherwise the
/// physical key decides, so a shifted digit (`!`) still reads as its digit
/// and a non-Latin letter as its key position. `KeyCode`'s derived names are
/// the W3C UI Events `code` values (`KeyA`, `Digit1`, `F12`, `ArrowLeft`),
/// parsed here rather than restating every variant; a test pins the mapping.
fn key_name(key_code: KeyCode, logical: &Key) -> Option<String> {
    if let Key::Character(text) = logical {
        let mut chars = text.chars();
        if let (Some(key), None) = (chars.next(), chars.next())
            && key.is_ascii_alphanumeric()
        {
            return Some(key.to_ascii_uppercase().to_string());
        }
    }
    if let Some(name) = logical_name(logical) {
        return Some(name.to_owned());
    }
    let code = format!("{key_code:?}");
    if let Some(rest) = code.strip_prefix("Key").or_else(|| code.strip_prefix("Digit"))
        && rest.len() == 1
    {
        return Some(rest.to_owned());
    }
    if code
        .strip_prefix('F')
        .is_some_and(|n| n.parse::<u8>().is_ok_and(|n| (1..=35).contains(&n)))
    {
        return Some(code);
    }
    Some(
        match key_code {
            KeyCode::ArrowLeft => "Left",
            KeyCode::ArrowRight => "Right",
            KeyCode::ArrowUp => "Up",
            KeyCode::ArrowDown => "Down",
            KeyCode::Tab => "Tab",
            KeyCode::Enter => "Return",
            KeyCode::Space => "space",
            KeyCode::Escape => "Escape",
            KeyCode::Home => "Home",
            KeyCode::End => "End",
            KeyCode::PageUp => "Page_Up",
            KeyCode::PageDown => "Page_Down",
            KeyCode::Insert => "Insert",
            KeyCode::Delete => "Delete",
            KeyCode::Backspace => "BackSpace",
            _ => return None,
        }
        .to_owned(),
    )
}

/// The canonical chord for one key press, in the form `conf.mix` ingestion
/// canonicalises to (`Ctrl+Alt+Shift+Super+KEY`). `None` for keys no binding
/// can name, including a bare modifier.
pub(crate) fn chord(key_code: KeyCode, logical: &Key, modifiers: Modifiers) -> Option<String> {
    let key = key_name(key_code, logical)?;
    let mut parts: Vec<&str> = [
        (modifiers.ctrl, "Ctrl"),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
        (modifiers.super_key, "Super"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    parts.push(&key);
    Some(parts.join("+"))
}

/// The configured action for a canonical chord. Ingestion refuses duplicate
/// chords, so at most one binding matches.
pub(crate) fn binding_action(config: &ShellConfig, chord: &str) -> Option<KeyAction> {
    for edge in Edge::ALL {
        let bindings = &config.bindings[edge.index()];
        for (key, mode) in [
            (&bindings.pin, PanelMode::Pinned),
            (&bindings.dock, PanelMode::Docked),
            (&bindings.hide, PanelMode::Hidden),
        ] {
            if key.as_deref() == Some(chord) {
                return Some(KeyAction::Mode(edge, mode));
            }
        }
    }
    (config.cycle_focus.as_deref() == Some(chord)).then_some(KeyAction::CycleFocus)
}

pub(crate) fn install(app: &mut App) {
    app.add_systems(
        Update,
        dispatch_bindings
            .in_set(ShellRuntimeSet::Input)
            .after(ConfigIngest)
            // A focus report staged in the same update applies first.
            .after(ShellStagedIngress),
    );
}

fn dispatch_bindings(
    mut keys: MessageReader<KeyboardInput>,
    mut held: Local<HeldModifiers>,
    config: Option<Res<ShellConfig>>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    for input in keys.read() {
        // Modifiers count from every event, repeats and enter-held ones too.
        if held.observe(input) {
            continue;
        }
        // Smoke hosts install no config: nothing is bound. A held repeat, or
        // a key already down when focus arrived (the host marks those
        // `repeat`), never fires: a binding that moves focus would otherwise
        // see its own chord again on the new surface.
        let Some(config) = config.as_deref() else {
            continue;
        };
        if input.state != ButtonState::Pressed || input.repeat {
            continue;
        }
        let Some(action) = chord(input.key_code, &input.logical_key, held.modifiers())
            .and_then(|chord| binding_action(config, &chord))
        else {
            continue;
        };
        // Targeting (`keyboard_target_output`: focused window's output, else
        // the pointer's) is trivial here: receiving the key proves a panel on
        // this output holds the keyboard. The pointer arm is for the future
        // compositor-grabbed route, which has no focused Quoin surface.
        let output = frame.0.geometry.output.clone();
        let at = time.elapsed();
        commands.write(match action {
            KeyAction::Mode(edge, mode) => {
                semantic_shell_command(output, at, edge, ShellSemanticVerb::PanelMode(mode))
            }
            // The same step `shell.focus.next` enqueues from the Bus.
            KeyAction::CycleFocus => focus_next_command(output, at),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, PanelInput, ShellModel};
    use cosmix_shell::runtime::{KeyboardCommand, ShellCommandKind, ShellRuntimePlugin};
    use std::time::Duration;

    const SUPER: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
        super_key: true,
    };
    const SUPER_SHIFT: Modifiers = Modifiers {
        shift: true,
        ..SUPER
    };

    fn app() -> App {
        let model = ShellModel::new(
            OutputKey::new("DP-1").unwrap(),
            LogicalSize::new(1000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let config = ShellConfig::parse(
            r#"{bindings: {
                left: {pin: "Super+Shift+Left", dock: "Super+Shift+D", hide: "Super+Shift+H"},
                right: {dock: "Super+F2"},
                top: {pin: "Alt+Q"},
                cycle_focus: "Super+Tab"}}"#,
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .add_message::<KeyboardInput>()
            .insert_resource(config);
        install(&mut app);
        app
    }

    fn key(key_code: KeyCode, logical_key: Key, state: ButtonState, repeat: bool) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key,
            state,
            text: None,
            repeat,
            window: Entity::PLACEHOLDER,
        }
    }

    /// Press and release one key inside `modifiers`, all in one update, with
    /// every event marked `repeat` when asked (a held repeat, or keys already
    /// down when focus arrived). Returns the commands produced.
    fn press(
        app: &mut App,
        key_code: KeyCode,
        logical_key: Key,
        modifiers: Modifiers,
        repeat: bool,
    ) -> Vec<ShellCommand> {
        let held: Vec<(KeyCode, Key)> = [
            (modifiers.ctrl, KeyCode::ControlLeft, Key::Control),
            (modifiers.alt, KeyCode::AltLeft, Key::Alt),
            (modifiers.shift, KeyCode::ShiftLeft, Key::Shift),
            (modifiers.super_key, KeyCode::SuperLeft, Key::Super),
        ]
        .into_iter()
        .filter_map(|(on, code, logical)| on.then_some((code, logical)))
        .collect();
        let world = app.world_mut();
        for (code, logical) in &held {
            world.write_message(key(*code, logical.clone(), ButtonState::Pressed, repeat));
        }
        world.write_message(key(key_code, logical_key.clone(), ButtonState::Pressed, repeat));
        world.write_message(key(key_code, logical_key, ButtonState::Released, false));
        for (code, logical) in held {
            world.write_message(key(code, logical, ButtonState::Released, false));
        }
        app.update();
        app.world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ShellCommand>>()
            .drain()
            .collect()
    }

    fn mode(app: &App, edge: Edge) -> PanelMode {
        app.world().resource::<ShellFrameState>().0.panel(edge).mode
    }

    #[test]
    fn per_edge_bindings_emit_mode_commands() {
        let mut app = app();
        let output = OutputKey::new("DP-1").unwrap();
        let mode_command = |edge, mode| ShellCommandKind::Panel {
            edge,
            input: PanelInput::SetMode(mode),
        };
        for (key_code, logical, modifiers, edge, expected) in [
            (
                KeyCode::ArrowLeft,
                Key::ArrowLeft,
                SUPER_SHIFT,
                Edge::Left,
                PanelMode::Pinned,
            ),
            // Letters follow the layout: the key labelled D, wherever it sits.
            (
                KeyCode::KeyE,
                Key::Character("D".into()),
                SUPER_SHIFT,
                Edge::Left,
                PanelMode::Docked,
            ),
            (
                KeyCode::KeyH,
                Key::Character("H".into()),
                SUPER_SHIFT,
                Edge::Left,
                PanelMode::Hidden,
            ),
            (KeyCode::F2, Key::F2, SUPER, Edge::Right, PanelMode::Docked),
        ] {
            let commands = press(&mut app, key_code, logical, modifiers, false);
            assert_eq!(commands.len(), 1, "{key_code:?}");
            // Targeting: the focused panel's output, i.e. this output.
            assert_eq!(commands[0].output, output);
            assert_eq!(commands[0].kind, mode_command(edge, expected));
            assert_eq!(mode(&app, edge), expected, "{key_code:?}");
        }
        // Right Alt as AltGr types a third-level character (AltGr+Q is `@` on
        // many layouts): that is not Alt+Q. As plain Alt, it is.
        for (logical, expected) in [(Key::AltGraph, PanelMode::Hidden), (Key::Alt, PanelMode::Pinned)] {
            let typed = if logical == Key::AltGraph { "@" } else { "q" };
            let world = app.world_mut();
            world.write_message(key(KeyCode::AltRight, logical.clone(), ButtonState::Pressed, false));
            world.write_message(key(
                KeyCode::KeyQ,
                Key::Character(typed.into()),
                ButtonState::Pressed,
                false,
            ));
            world.write_message(key(KeyCode::KeyQ, Key::Character(typed.into()), ButtonState::Released, false));
            world.write_message(key(KeyCode::AltRight, logical, ButtonState::Released, false));
            app.update();
            app.world_mut()
                .resource_mut::<bevy::ecs::message::Messages<ShellCommand>>()
                .clear();
            assert_eq!(mode(&app, Edge::Top), expected);
        }

        // The right dock never touched the left edge, and vice versa.
        assert_eq!(mode(&app, Edge::Left), PanelMode::Hidden);

        // Unbound chords, repeats and bare keys emit nothing.
        assert!(press(&mut app, KeyCode::ArrowLeft, Key::ArrowLeft, SUPER, false).is_empty());
        assert!(
            press(
                &mut app,
                KeyCode::ArrowLeft,
                Key::ArrowLeft,
                SUPER_SHIFT,
                true
            )
            .is_empty()
        );
        assert!(
            press(
                &mut app,
                KeyCode::ArrowLeft,
                Key::ArrowLeft,
                Modifiers::default(),
                false
            )
            .is_empty()
        );
        assert_eq!(mode(&app, Edge::Left), PanelMode::Hidden);

        // The whole chord already held when focus arrived (the host marks it
        // `repeat`) does not fire: a focus move the cycle itself caused would
        // otherwise re-deliver Super+Tab and walk every stop in one press.
        assert!(press(&mut app, KeyCode::Tab, Key::Tab, SUPER, true).is_empty());
        // A modifier still held from before focus arrived counts; the key
        // pressed afresh inside it fires.
        {
            let world = app.world_mut();
            world.write_message(key(KeyCode::SuperRight, Key::Super, ButtonState::Pressed, true));
            world.write_message(key(KeyCode::Tab, Key::Tab, ButtonState::Pressed, false));
            world.write_message(key(KeyCode::Tab, Key::Tab, ButtonState::Released, false));
            world.write_message(key(KeyCode::SuperRight, Key::Super, ButtonState::Released, false));
        }
        app.update();
        let fired: Vec<ShellCommand> = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ShellCommand>>()
            .drain()
            .collect();
        assert_eq!(
            fired.iter().map(|c| &c.kind).collect::<Vec<_>>(),
            [&ShellCommandKind::Keyboard(KeyboardCommand::CycleFocus)]
        );
        // Modifier state follows event order, not the end of the frame: a
        // Super released before Tab is pressed does not make Super+Tab.
        {
            let world = app.world_mut();
            world.write_message(key(KeyCode::SuperLeft, Key::Super, ButtonState::Pressed, false));
            world.write_message(key(KeyCode::SuperLeft, Key::Super, ButtonState::Released, false));
            world.write_message(key(KeyCode::Tab, Key::Tab, ButtonState::Pressed, false));
        }
        app.update();
        assert!(
            app.world_mut()
                .resource_mut::<bevy::ecs::message::Messages<ShellCommand>>()
                .drain()
                .next()
                .is_none()
        );
        app.world_mut()
            .write_message(key(KeyCode::Tab, Key::Tab, ButtonState::Released, false));

        let cycle = press(&mut app, KeyCode::Tab, Key::Tab, SUPER, false);
        assert_eq!(
            cycle.iter().map(|c| &c.kind).collect::<Vec<_>>(),
            [&ShellCommandKind::Keyboard(KeyboardCommand::CycleFocus)]
        );
    }

    #[test]
    fn chords_match_the_config_canonical_form() {
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        };
        let all = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            super_key: true,
        };
        let cases = [
            (
                KeyCode::Digit1,
                Key::Character("!".into()),
                ctrl_shift,
                "Ctrl+Shift+1",
            ),
            // Keypad keys with NumLock off match by their logical value.
            (KeyCode::Numpad4, Key::ArrowLeft, SUPER, "Super+Left"),
            (KeyCode::NumpadEnter, Key::Enter, SUPER, "Super+Return"),
            (KeyCode::Numpad7, Key::Character("7".into()), SUPER, "Super+7"),
            (KeyCode::KeyA, Key::Character("a".into()), SUPER, "Super+A"),
            // A non-Latin letter falls back to the physical key.
            (KeyCode::KeyQ, Key::Character("й".into()), SUPER, "Super+Q"),
            (KeyCode::ArrowLeft, Key::ArrowLeft, SUPER_SHIFT, "Shift+Super+Left"),
            (KeyCode::F35, Key::F35, all, "Ctrl+Alt+Shift+Super+F35"),
            (KeyCode::PageDown, Key::PageDown, SUPER, "Super+Page_Down"),
            (KeyCode::Enter, Key::Enter, SUPER, "Super+Return"),
            (KeyCode::Space, Key::Space, SUPER, "Super+space"),
            (KeyCode::Backspace, Key::Backspace, SUPER, "Super+BackSpace"),
        ];
        for (key_code, logical, modifiers, expected) in cases {
            assert_eq!(
                chord(key_code, &logical, modifiers).as_deref(),
                Some(expected)
            );
            // Every produced chord is one conf.mix accepts unchanged.
            let source = format!(r#"{{bindings: {{cycle_focus: "{expected}"}}}}"#);
            assert_eq!(
                ShellConfig::parse(&source).unwrap().cycle_focus.as_deref(),
                Some(expected)
            );
        }
        let unidentified = Key::Unidentified(bevy::input::keyboard::NativeKey::Unidentified);
        for bare in [KeyCode::SuperLeft, KeyCode::ShiftRight, KeyCode::NumpadEnter] {
            assert_eq!(chord(bare, &unidentified, SUPER), None);
        }
    }
}
