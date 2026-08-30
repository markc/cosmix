//! Studio-local settings panel. This is deliberately one real, session-scoped
//! setting; CTK owns modal capture and file selection, while Studio owns the
//! setting's meaning and presentation.

use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With, Without};
use bevy::ecs::system::SystemParam;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor, UiTheme};
use bevy::input_focus::tab_navigation::TabGroup;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{percent, px, FocusPolicy, GlobalZIndex, Pressed, UiRect};
use bevy::ui_widgets::Activate;
use cosmix_actions::{studio as ids, ActionId};
use ctk::prelude::{
    action_button, ActionRequest, ModalCapture, ModalCaptureLayer, ModalCaptureOwner,
    ModalCaptureToken, Source,
};
use ctk::theme::{ctk_color, tokens};

use crate::action::{ActionProduce, ActionRoute, CaptureEstablishedThisFrame};
use crate::editor::SongEditor;

const SETTINGS_Z: i32 = 900;
const SETTINGS_LAYER: ModalCaptureLayer = ModalCaptureLayer(SETTINGS_Z);

/// Canonical menu ids consumed by this module's action-bus reader.
pub(crate) const HANDLED_MENU_ACTION_IDS: &[ActionId] = &[ids::MENU_SETTINGS];

#[derive(Clone, Copy)]
enum SettingsAction {
    Open,
    Close,
    Activate,
}

impl SettingsAction {
    fn from_action(action: ActionId) -> Option<Self> {
        match action {
            ids::MENU_SETTINGS => Some(Self::Open),
            ids::SETTINGS_CLOSE => Some(Self::Close),
            ids::SETTINGS_ACTIVATE => Some(Self::Activate),
            _ => None,
        }
    }
}

pub(crate) fn handles_menu_action(action: ActionId) -> bool {
    let executable = matches!(
        SettingsAction::from_action(action),
        Some(SettingsAction::Open)
    );
    debug_assert_eq!(HANDLED_MENU_ACTION_IDS.contains(&action), executable);
    executable
}

#[derive(Resource, Default)]
pub(crate) struct SettingsState {
    active: Option<ActiveSettings>,
}

struct ActiveSettings {
    root: Entity,
    path_text: Entity,
    token: ModalCaptureToken,
    /// Focus held before menu chrome cleared it. Restoration validates that
    /// the entity still exists because a modal may outlive its invoker.
    previous_focus: Option<Entity>,
}

#[derive(Component)]
struct SettingsRoot;

#[derive(Component)]
struct SettingsPanel;

#[derive(Component, Clone, Copy)]
struct SettingsButton(SettingsButtonKind);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsButtonKind {
    ChooseSoundfont,
    Close,
}

pub(crate) struct SettingsPlugin;

impl Plugin for SettingsPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<ctk::prelude::ModalCapturePlugin>() {
            app.add_plugins(ctk::prelude::ModalCapturePlugin);
        }
        app.init_resource::<SettingsState>()
            .init_resource::<CaptureEstablishedThisFrame>()
            .add_message::<ActionRequest>()
            .add_observer(on_settings_button)
            .add_systems(Update, on_settings_actions.in_set(ActionRoute))
            .add_systems(
                Update,
                (
                    sync_soundfont_path.after(ActionProduce),
                    update_settings_styles,
                ),
            );
    }
}

#[derive(SystemParam)]
struct SettingsActionParams<'w, 's> {
    actions: MessageReader<'w, 's, ActionRequest>,
    editor: Option<Res<'w, SongEditor>>,
    state: ResMut<'w, SettingsState>,
    capture: ResMut<'w, ModalCapture>,
    focus: ResMut<'w, InputFocus>,
    buttons: Query<'w, 's, (), With<SettingsButton>>,
    capture_established: ResMut<'w, CaptureEstablishedThisFrame>,
    commands: Commands<'w, 's>,
}

fn on_settings_actions(mut params: SettingsActionParams) {
    for request in params.actions.read() {
        let Some(action) = SettingsAction::from_action(request.action) else {
            continue;
        };
        match action {
            SettingsAction::Open if params.state.active.is_none() => {
                let previous_focus = request.invocation_focus.or_else(|| params.focus.get());
                let (root, path_text, close) =
                    spawn_settings(&mut params.commands, soundfont_path(&params.editor));
                let token = params.capture.acquire(
                    ModalCaptureOwner {
                        kind: "studio.settings",
                        entity: Some(root),
                    },
                    SETTINGS_LAYER,
                );
                params.focus.set(close, FocusCause::Navigated);
                params.state.active = Some(ActiveSettings {
                    root,
                    path_text,
                    token,
                    previous_focus,
                });
                params.capture_established.mark_request(request);
            }
            SettingsAction::Close => {
                let is_top = params
                    .state
                    .active
                    .as_ref()
                    .is_some_and(|active| params.capture.is_top(active.token));
                if is_top {
                    close_settings(
                        &mut params.state,
                        &mut params.capture,
                        &mut params.focus,
                        &mut params.commands,
                    );
                }
            }
            SettingsAction::Activate => {
                let is_top = params
                    .state
                    .active
                    .as_ref()
                    .is_some_and(|active| params.capture.is_top(active.token));
                if is_top {
                    if let Some(entity) = params
                        .focus
                        .get()
                        .filter(|entity| params.buttons.contains(*entity))
                    {
                        params.commands.trigger(Activate { entity });
                    }
                }
            }
            SettingsAction::Open => {}
        }
    }
}

fn on_settings_button(
    activated: On<Activate>,
    buttons: Query<&SettingsButton>,
    mut state: ResMut<SettingsState>,
    mut capture: ResMut<ModalCapture>,
    mut focus: ResMut<InputFocus>,
    mut actions: MessageWriter<ActionRequest>,
    mut commands: Commands,
) {
    let Ok(button) = buttons.get(activated.entity) else {
        return;
    };
    let Some(active) = state.active.as_ref() else {
        return;
    };
    if !capture.is_top(active.token) {
        return;
    }

    match button.0 {
        SettingsButtonKind::ChooseSoundfont => {
            actions.write(ActionRequest {
                action: ids::MENU_SF_OPEN,
                // `Activate` carries no pointer/key provenance; retain the
                // pre-bridge programmatic-key contract for this path.
                source: Source::Key,
                args: Default::default(),
                invocation_focus: None,
            });
        }
        SettingsButtonKind::Close => {
            close_settings(&mut state, &mut capture, &mut focus, &mut commands);
        }
    }
}

fn close_settings(
    state: &mut SettingsState,
    capture: &mut ModalCapture,
    focus: &mut InputFocus,
    commands: &mut Commands,
) {
    let Some(active) = state.active.take() else {
        return;
    };
    let released = capture.release_latched(active.token);
    debug_assert!(released, "settings capture token was not live");
    commands.entity(active.root).despawn();
    if let Some(previous) = active
        .previous_focus
        .filter(|entity| commands.get_entity(*entity).is_ok())
    {
        focus.set(previous, FocusCause::Navigated);
    } else {
        focus.clear();
    }
}

fn sync_soundfont_path(
    state: Res<SettingsState>,
    editor: Option<Res<SongEditor>>,
    mut texts: Query<&mut Text>,
) {
    let Some(active) = state.active.as_ref() else {
        return;
    };
    let Ok(mut text) = texts.get_mut(active.path_text) else {
        return;
    };
    let path = soundfont_path(&editor);
    if text.0 != path {
        text.0 = path;
    }
}

fn soundfont_path(editor: &Option<Res<SongEditor>>) -> String {
    editor
        .as_deref()
        .and_then(SongEditor::soundfont_source)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn spawn_settings(commands: &mut Commands, soundfont: String) -> (Entity, Entity, Entity) {
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                top: px(0),
                bottom: px(0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            ThemeBackgroundColor(tokens::SCRIM),
            GlobalZIndex(SETTINGS_Z),
            FocusPolicy::Block,
            TabGroup::modal(),
            SettingsRoot,
        ))
        .id();

    let panel = commands
        .spawn((
            Node {
                width: percent(58),
                max_width: px(810),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                padding: UiRect::all(px(12)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(7)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            ThemeBorderColor(tokens::BORDER),
            SettingsPanel,
        ))
        .id();

    let title = settings_text(commands, "Settings", 18.0, false);
    let label = settings_text(commands, "SoundFont (session)", 13.0, true);
    let path_text = settings_text(commands, &soundfont, 13.0, false);
    commands.entity(path_text).insert(Node {
        flex_grow: 1.0,
        min_width: px(0),
        ..default()
    });
    let choose = settings_button(commands, "Choose…", SettingsButtonKind::ChooseSoundfont);
    let row = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .add_children(&[label, path_text, choose])
        .id();
    let close = settings_button(commands, "Close", SettingsButtonKind::Close);
    let actions = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::End,
            column_gap: px(7),
            ..default()
        })
        .add_child(close)
        .id();

    commands.entity(panel).add_children(&[title, row, actions]);
    commands.entity(root).add_child(panel);
    (root, path_text, close)
}

fn settings_button(commands: &mut Commands, label: &str, kind: SettingsButtonKind) -> Entity {
    let label = settings_text(commands, label, 13.0, false);
    let id = match kind {
        SettingsButtonKind::ChooseSoundfont => "studio.settings.choose-soundfont",
        SettingsButtonKind::Close => "studio.settings.close",
    };
    commands
        .spawn((action_button(id, 72.0, 30.0), SettingsButton(kind)))
        .add_child(label)
        .id()
}

fn settings_text(commands: &mut Commands, value: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(size),
            ThemeTextColor(if dim { tokens::TEXT_DIM } else { tokens::TEXT }),
        ))
        .id()
}

#[allow(clippy::type_complexity)]
fn update_settings_styles(
    theme: Res<UiTheme>,
    mut panels: Query<
        (&mut BackgroundColor, &mut BorderColor),
        (With<SettingsPanel>, Without<SettingsButton>),
    >,
    mut buttons: Query<
        (&Hovered, Has<Pressed>, &mut BackgroundColor),
        (With<SettingsButton>, Without<SettingsPanel>),
    >,
) {
    for (mut background, mut border) in &mut panels {
        background.0 = ctk_color(&theme, &tokens::PANEL);
        border.set_all(ctk_color(&theme, &tokens::BORDER));
    }
    for (hovered, pressed, mut background) in &mut buttons {
        background.0 = if pressed {
            ctk_color(&theme, &tokens::CONTROL_ACTIVE)
        } else if hovered.get() {
            ctk_color(&theme, &tokens::THUMB)
        } else {
            ctk_color(&theme, &tokens::CONTROL)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::app::TaskPoolPlugin;
    use bevy::input::keyboard::{Key, KeyboardInput};
    use bevy::input::{ButtonState, InputPlugin};
    use bevy::input_focus::tab_navigation::TabIndex;
    use bevy::input_focus::tab_navigation::TabNavigationPlugin;
    use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
    use bevy::ui_widgets::ButtonPlugin;
    use bevy::window::{PrimaryWindow, Window};
    use cosmix_actions::ActionId;
    use ctk::prelude::{
        CtkWidgetsPlugin, FileRequest, FileRequestId, FileRequesterPlugin, FileRequesterSystems,
        ModalCaptureSystems, MusicdMixerState, TransportSeekRequest,
    };

    use crate::action::{ActionPlugin, ActionProduce};

    #[derive(Resource, Default)]
    struct CaptureSnapshots(Vec<(Option<&'static str>, bool)>);

    #[derive(Resource, Default)]
    struct SeenActions(Vec<ActionId>);

    fn record_actions(mut requests: MessageReader<ActionRequest>, mut seen: ResMut<SeenActions>) {
        seen.0.extend(requests.read().map(|request| request.action));
    }

    fn keyboard_settings_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            TabNavigationPlugin,
            ButtonPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<UiTheme>()
        .init_resource::<MusicdMixerState>()
        .init_resource::<SeenActions>()
        .add_message::<TransportSeekRequest>()
        .add_plugins((SettingsPlugin, ActionPlugin))
        .add_systems(Update, record_actions.after(ActionProduce));

        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.finish();
        app.cleanup();
        app.update();

        write_action(&mut app, ids::MENU_SETTINGS, None);
        app.update();
        app.world_mut().resource_mut::<SeenActions>().0.clear();
        (app, window)
    }

    fn settings_button_entity(app: &mut App, kind: SettingsButtonKind) -> Entity {
        let mut buttons = app.world_mut().query::<(Entity, &SettingsButton)>();
        buttons
            .iter(app.world())
            .find_map(|(entity, button)| (button.0 == kind).then_some(entity))
            .unwrap()
    }

    fn send_key(
        app: &mut App,
        window: Entity,
        key_code: KeyCode,
        logical_key: Key,
        state: ButtonState,
    ) {
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key,
            state,
            text: None,
            repeat: false,
            window,
        });
    }

    fn test_soundfont_request(
        mut actions: MessageReader<ActionRequest>,
        mut requests: MessageWriter<FileRequest>,
    ) {
        for action in actions.read() {
            if action.action == ids::MENU_SF_OPEN {
                let mut request = FileRequest::open_file(FileRequestId(900), "Open SoundFont");
                request.initial_directory = Some(std::env::temp_dir());
                requests.write(request);
            }
        }
    }

    fn write_action(app: &mut App, action: ActionId, invocation_focus: Option<Entity>) {
        app.world_mut().write_message(ActionRequest {
            action,
            source: Source::Menu,
            args: Default::default(),
            invocation_focus,
        });
    }

    fn capture_snapshot(
        capture: Res<ModalCapture>,
        settings: Res<SettingsState>,
        mut snapshots: ResMut<CaptureSnapshots>,
    ) {
        snapshots.0.push((
            capture.top_owner().map(|owner| owner.kind),
            settings.active.is_some(),
        ));
    }

    #[test]
    fn requester_escape_close_frame_preserves_settings_then_settings_escape_closes() {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<CaptureSnapshots>()
            .add_plugins((FileRequesterPlugin, SettingsPlugin))
            .add_systems(Update, test_soundfont_request.before(FileRequesterSystems))
            .add_systems(Update, capture_snapshot.after(ModalCaptureSystems));
        app.finish();
        app.cleanup();

        write_action(&mut app, ids::MENU_SETTINGS, None);
        app.update();
        let settings_token = app
            .world()
            .resource::<SettingsState>()
            .active
            .as_ref()
            .unwrap()
            .token;
        assert!(app
            .world()
            .resource::<ModalCapture>()
            .is_top(settings_token));

        let mut buttons = app.world_mut().query::<(Entity, &SettingsButton)>();
        let choose = buttons
            .iter(app.world())
            .find_map(|(entity, button)| {
                matches!(button.0, SettingsButtonKind::ChooseSoundfont).then_some(entity)
            })
            .unwrap();
        app.world_mut().trigger(Activate { entity: choose });
        app.world_mut().flush();
        app.update();
        assert_eq!(
            app.world().resource::<ModalCapture>().top_owner(),
            Some(ModalCaptureOwner {
                kind: "ctk.modal-coordinator",
                entity: None,
            })
        );

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();

        assert_eq!(
            app.world().resource::<CaptureSnapshots>().0.last(),
            Some(&(Some("ctk.modal-coordinator"), true)),
            "the requester remains top and settings stays open through its close frame"
        );
        assert!(app.world().resource::<SettingsState>().active.is_some());
        assert!(app
            .world()
            .resource::<ModalCapture>()
            .is_top(settings_token));

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        write_action(&mut app, ids::SETTINGS_CLOSE, None);
        app.update();

        assert_eq!(
            app.world().resource::<CaptureSnapshots>().0.last(),
            Some(&(Some("studio.settings"), false)),
            "settings releases into the close-frame latch when it is top"
        );
        assert!(app.world().resource::<SettingsState>().active.is_none());
        assert!(!app.world().resource::<ModalCapture>().is_captured());
    }

    #[test]
    fn focused_close_space_closes_settings_without_transport_toggle() {
        let (mut app, window) = keyboard_settings_app();
        let close = settings_button_entity(&mut app, SettingsButtonKind::Close);
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(close));

        send_key(
            &mut app,
            window,
            KeyCode::Space,
            Key::Space,
            ButtonState::Pressed,
        );
        app.update();

        assert!(app.world().resource::<SettingsState>().active.is_none());
        assert!(app.world().resource::<SeenActions>().0.is_empty());
    }

    #[test]
    fn escape_then_space_does_not_activate_despawned_settings_button() {
        let (mut app, window) = keyboard_settings_app();
        let choose = settings_button_entity(&mut app, SettingsButtonKind::ChooseSoundfont);
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(choose, FocusCause::Navigated);
        send_key(
            &mut app,
            window,
            KeyCode::Escape,
            Key::Escape,
            ButtonState::Pressed,
        );
        send_key(
            &mut app,
            window,
            KeyCode::Space,
            Key::Space,
            ButtonState::Pressed,
        );

        app.update();

        assert!(app.world().resource::<SettingsState>().active.is_none());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::MENU_SF_OPEN));
    }

    #[test]
    fn settings_close_restores_live_menu_invocation_focus() {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .add_plugins(SettingsPlugin);
        app.finish();
        app.cleanup();

        let board_control = app.world_mut().spawn((Button, TabIndex(0))).id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(board_control, FocusCause::Navigated);
        // Menu chrome clears focus before activation. The event carries the
        // pre-menu snapshot across that hand-off.
        app.world_mut().resource_mut::<InputFocus>().clear();
        write_action(&mut app, ids::MENU_SETTINGS, Some(board_control));
        app.update();

        let close = settings_button_entity(&mut app, SettingsButtonKind::Close);
        app.world_mut().trigger(Activate { entity: close });
        app.world_mut().flush();

        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(board_control)
        );
    }

    #[test]
    fn settings_close_clears_focus_when_menu_invoker_was_despawned() {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .add_plugins(SettingsPlugin);
        app.finish();
        app.cleanup();

        let board_control = app.world_mut().spawn((Button, TabIndex(0))).id();
        write_action(&mut app, ids::MENU_SETTINGS, Some(board_control));
        app.update();
        app.world_mut().despawn(board_control);

        let close = settings_button_entity(&mut app, SettingsButtonKind::Close);
        app.world_mut().trigger(Activate { entity: close });
        app.world_mut().flush();

        assert_eq!(app.world().resource::<InputFocus>().get(), None);
    }

    #[test]
    fn settings_modal_tab_and_shift_tab_wrap_between_buttons() {
        let (mut app, window) = keyboard_settings_app();
        let choose = settings_button_entity(&mut app, SettingsButtonKind::ChooseSoundfont);
        let close = settings_button_entity(&mut app, SettingsButtonKind::Close);
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(close));

        send_key(
            &mut app,
            window,
            KeyCode::Tab,
            Key::Tab,
            ButtonState::Pressed,
        );
        app.update();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(choose),
            "Tab from the final button must wrap to the first"
        );

        send_key(
            &mut app,
            window,
            KeyCode::Tab,
            Key::Tab,
            ButtonState::Released,
        );
        app.update();
        send_key(
            &mut app,
            window,
            KeyCode::ShiftLeft,
            Key::Shift,
            ButtonState::Pressed,
        );
        send_key(
            &mut app,
            window,
            KeyCode::Tab,
            Key::Tab,
            ButtonState::Pressed,
        );
        app.update();
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(close),
            "Shift-Tab from the first button must wrap to the final button"
        );
    }
}
