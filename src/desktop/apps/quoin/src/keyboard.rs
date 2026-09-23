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
//! [`keyboard_target_output`] rule fed by comp's focus and pointer reports.
//!
//! Mode bindings dispatch through the precise mode verb, never the legacy
//! pin/unpin pair, so a keyboard pin is exactly a menu or Bus pin.

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::time::Real;
use cosmix_shell::core::{Edge, PanelMode, keyboard_target_output};
use cosmix_shell::runtime::{
    KeyboardCommand, ShellCommand, ShellCommandKind, ShellFrameState, ShellRuntimeSet,
    ShellSemanticVerb, semantic_shell_command,
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

impl Modifiers {
    fn held(keys: &ButtonInput<KeyCode>) -> Self {
        Self {
            ctrl: keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
            alt: keys.any_pressed([KeyCode::AltLeft, KeyCode::AltRight]),
            shift: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
            super_key: keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]),
        }
    }
}

/// A binding's key name in the config grammar. Letters follow the layout
/// (the logical key, as comp's own bindings do), so `Super+Shift+D` is the
/// key labelled D; everything else is the physical key, so a shifted digit
/// still reads as its digit. `KeyCode`'s derived names are the W3C UI Events
/// `code` values (`KeyA`, `Digit1`, `F12`, `ArrowLeft`), parsed here rather
/// than restating every variant; a test pins the mapping.
fn key_name(key_code: KeyCode, logical: &Key) -> Option<String> {
    if let Key::Character(text) = logical {
        let mut chars = text.chars();
        if let (Some(letter), None) = (chars.next(), chars.next())
            && letter.is_ascii_alphabetic()
        {
            return Some(letter.to_ascii_uppercase().to_string());
        }
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
            .after(ConfigIngest),
    );
}

fn dispatch_bindings(
    mut keys: MessageReader<KeyboardInput>,
    held: Res<ButtonInput<KeyCode>>,
    config: Option<Res<ShellConfig>>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    // Smoke hosts install no config: nothing is bound.
    let Some(config) = config else {
        keys.clear();
        return;
    };
    for input in keys.read() {
        // Holding a binding must not re-fire it at the repeat rate.
        if input.state != ButtonState::Pressed || input.repeat {
            continue;
        }
        let Some(action) = chord(input.key_code, &input.logical_key, Modifiers::held(&held))
            .and_then(|chord| binding_action(&config, &chord))
        else {
            continue;
        };
        // Receiving the key proves a panel on this output holds the keyboard,
        // so the focused window's output wins and the pointer is not needed.
        let Some(output) = keyboard_target_output(Some(&frame.0.geometry.output), None).cloned()
        else {
            continue;
        };
        let at = time.elapsed();
        commands.write(match action {
            KeyAction::Mode(edge, mode) => {
                semantic_shell_command(output, at, edge, ShellSemanticVerb::PanelMode(mode))
            }
            KeyAction::CycleFocus => ShellCommand {
                output,
                at,
                kind: ShellCommandKind::Keyboard(KeyboardCommand::CycleFocus),
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_shell::core::{LogicalSize, OutputKey, PanelInput, ShellModel};
    use cosmix_shell::runtime::ShellRuntimePlugin;
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
                cycle_focus: "Super+Tab"}}"#,
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .add_message::<KeyboardInput>()
            .init_resource::<ButtonInput<KeyCode>>()
            .insert_resource(config);
        install(&mut app);
        app
    }

    /// Press one key with `modifiers` held; returns the commands it produced.
    fn press(
        app: &mut App,
        key_code: KeyCode,
        logical_key: Key,
        modifiers: Modifiers,
        repeat: bool,
    ) -> Vec<ShellCommand> {
        {
            let mut held = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            held.release_all();
            for (on, key) in [
                (modifiers.ctrl, KeyCode::ControlLeft),
                (modifiers.alt, KeyCode::AltRight),
                (modifiers.shift, KeyCode::ShiftLeft),
                (modifiers.super_key, KeyCode::SuperLeft),
            ] {
                if on {
                    held.press(key);
                }
            }
        }
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: None,
            repeat,
            window: Entity::PLACEHOLDER,
        });
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

        let cycle = press(&mut app, KeyCode::Tab, Key::Tab, SUPER, false);
        assert_eq!(
            cycle.iter().map(|c| &c.kind).collect::<Vec<_>>(),
            [&ShellCommandKind::Keyboard(KeyboardCommand::CycleFocus)]
        );
    }

    #[test]
    fn chords_match_the_config_canonical_form() {
        let shift = Modifiers {
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
            (KeyCode::Digit1, Key::Character("!".into()), shift, "Shift+1"),
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
