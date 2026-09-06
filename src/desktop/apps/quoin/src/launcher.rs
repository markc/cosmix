//! One asynchronous argv-based launcher, with an optional operator Mix hook.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{InteractionDisabled, px};
use bevy::ui_widgets::{Activate, Button as WidgetButton};
use cosmix_shell::core::Edge;
use cosmix_shell::runtime::{ShellFrameState, ShellRuntimeSet};
use cosmix_shell_host::LayerHostWake;
use ctk::theme::tokens;

pub(crate) struct LauncherPlugin;

impl Plugin for LauncherPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LauncherState>()
            .add_observer(activate)
            .add_systems(
                Update,
                (start_request, present)
                    .chain()
                    .in_set(ShellRuntimeSet::Host),
            );
    }
}

#[derive(Component)]
struct ThunderbirdButton;

#[derive(Component)]
struct LauncherLabel;

#[derive(Debug)]
enum LaunchFeedback {
    Started,
    Finished(Result<(), String>),
}

#[derive(Resource, Default)]
struct LauncherState {
    requested: bool,
    busy: bool,
    feedback: Arc<Mutex<Vec<LaunchFeedback>>>,
    label: Option<String>,
}

fn launch_argv(hook: Option<&OsStr>) -> Result<Vec<OsString>, String> {
    match hook {
        None => Ok(vec!["thunderbird".into()]),
        Some(hook) if Path::new(hook).is_absolute() => Ok(vec![
            // The operator hook is Mix source, never shell source. This is the
            // installed runtime, independent of the public source checkout.
            "/opt/cosmix/bin/mix".into(),
            hook.to_owned(),
            "thunderbird".into(),
        ]),
        Some(_) => Err("COSMIX_QUOIN_LAUNCHER must be an absolute Mix script path".into()),
    }
}

fn send_feedback(queue: &Mutex<Vec<LaunchFeedback>>, wake: &dyn Fn(), feedback: LaunchFeedback) {
    queue
        .lock()
        .expect("launcher feedback lock poisoned")
        .push(feedback);
    wake();
}

fn run_launcher(argv: &[OsString], started: impl FnOnce()) -> Result<(), String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        // Preserve diagnostics in the normal service journal, without an
        // unbounded pipe or blocking the UI while the application is alive.
        .spawn()
        .map_err(|error| format!("could not start Thunderbird: {error}"))?;
    started();
    let status = child
        .wait()
        .map_err(|error| format!("could not wait for Thunderbird launcher: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Thunderbird launcher exited with {status}"))
    }
}

fn visible(frame: &ShellFrameState) -> bool {
    let panel = frame.0.panel(Edge::Bottom);
    panel.mapped && panel.active_page_id.as_deref() == Some("launcher")
}

fn activate(
    event: On<Activate>,
    buttons: Query<Has<InteractionDisabled>, With<ThunderbirdButton>>,
    frame: Res<ShellFrameState>,
    mut state: ResMut<LauncherState>,
) {
    let Ok(disabled) = buttons.get(event.entity) else {
        return;
    };
    if disabled || !visible(&frame) || state.busy {
        return;
    }
    state.requested = true;
    state.busy = true;
    state.label = Some("Starting Thunderbird…".into());
}

fn start_request(mut state: ResMut<LauncherState>, wake: Res<LayerHostWake>) {
    if !std::mem::take(&mut state.requested) {
        return;
    }
    let argv = match launch_argv(std::env::var_os("COSMIX_QUOIN_LAUNCHER").as_deref()) {
        Ok(argv) => argv,
        Err(error) => {
            eprintln!("QUOIN_LAUNCH_FAILED app=thunderbird error={error}");
            state.busy = false;
            state.label = Some(error);
            (wake.callback())();
            return;
        }
    };
    let feedback = state.feedback.clone();
    let wake_callback = wake.callback();
    let result = std::thread::Builder::new()
        .name("quoin-launcher".into())
        .spawn(move || {
            let result = run_launcher(&argv, || {
                send_feedback(&feedback, &*wake_callback, LaunchFeedback::Started);
            });
            send_feedback(&feedback, &*wake_callback, LaunchFeedback::Finished(result));
        });
    if let Err(error) = result {
        state.busy = false;
        state.label = Some(format!("Could not start launcher worker: {error}"));
        eprintln!("QUOIN_LAUNCH_FAILED app=thunderbird error={error}");
    }
    (wake.callback())();
}

fn present(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    mut state: ResMut<LauncherState>,
    mut focus: ResMut<InputFocus>,
    mut buttons: Query<(Entity, &mut TabIndex, Has<InteractionDisabled>), With<ThunderbirdButton>>,
    mut labels: Query<&mut Text, With<LauncherLabel>>,
) {
    // Called on host events; workers wake the host when feedback arrives.
    // There is no timer or process-status polling loop.
    let feedback = std::mem::take(
        &mut *state
            .feedback
            .lock()
            .expect("launcher feedback lock poisoned"),
    );
    for feedback in feedback {
        match feedback {
            LaunchFeedback::Started => {
                println!("QUOIN_LAUNCH_STARTED app=thunderbird");
                state.label = Some("Thunderbird started".into());
            }
            LaunchFeedback::Finished(result) => {
                state.busy = false;
                state.label = Some(match result {
                    Ok(()) => {
                        println!("QUOIN_LAUNCH_FINISHED app=thunderbird status=success");
                        "Thunderbird".into()
                    }
                    Err(error) => {
                        eprintln!("QUOIN_LAUNCH_FAILED app=thunderbird error={error}");
                        error
                    }
                });
            }
        }
    }
    if let Some(label) = state.label.take() {
        for mut text in &mut labels {
            text.0.clone_from(&label);
        }
    }
    let enabled = visible(&frame) && !state.busy;
    for (entity, mut tab, disabled) in &mut buttons {
        tab.0 = if enabled { 0 } else { -1 };
        if enabled && disabled {
            commands.entity(entity).remove::<InteractionDisabled>();
        } else if !enabled && !disabled {
            commands.entity(entity).insert(InteractionDisabled);
        }
        if !enabled && focus.get() == Some(entity) {
            focus.clear();
        }
    }
}

pub(crate) fn thunderbird_button(commands: &mut Commands) -> Entity {
    let label = commands
        .spawn((
            Text::new("Thunderbird"),
            TextFont::from_font_size(13.0),
            bevy::feathers::theme::ThemeTextColor(tokens::TEXT),
            LauncherLabel,
        ))
        .id();
    commands
        .spawn((
            Node {
                min_height: px(28),
                padding: UiRect::axes(px(8), px(4)),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            WidgetButton,
            Pickable::default(),
            Hovered::default(),
            TabIndex(-1),
            InteractionDisabled,
            bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
            ThunderbirdButton,
        ))
        .add_child(label)
        .id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_argv_preserves_hook_as_one_argument_and_rejects_relative_paths() {
        assert_eq!(
            launch_argv(None).unwrap(),
            vec![OsString::from("thunderbird")]
        );
        assert_eq!(
            launch_argv(Some(OsStr::new("/tmp/mail hook.mix"))).unwrap(),
            ["/opt/cosmix/bin/mix", "/tmp/mail hook.mix", "thunderbird"].map(OsString::from)
        );
        assert!(launch_argv(Some(OsStr::new("relative.mix"))).is_err());
        assert!(launch_argv(Some(OsStr::new(""))).is_err());
    }

    #[test]
    fn spawn_failure_never_reports_started() {
        let result = run_launcher(
            &["/nonexistent-quoin-launcher-test/executable".into()],
            || {
                panic!("failed spawn must not announce startup");
            },
        );
        assert!(result.unwrap_err().contains("could not start Thunderbird"));
    }

    #[test]
    fn button_uses_widget_type_and_tracks_visible_page_and_focus() {
        use bevy::ecs::system::RunSystemOnce;
        use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
        use cosmix_shell::runtime::ShellFrame;
        use std::time::Duration;
        let model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1536.0, 864.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        app.insert_resource(ShellFrameState(ShellFrame::from_model(&model)))
            .init_resource::<LauncherState>()
            .init_resource::<InputFocus>()
            .add_observer(activate);
        let mut queue = bevy::ecs::world::CommandQueue::default();
        let entity = thunderbird_button(&mut Commands::new(&mut queue, app.world()));
        queue.apply(app.world_mut());
        assert!(app.world().entity(entity).contains::<WidgetButton>());
        app.world_mut().run_system_once(present).unwrap();
        assert!(app.world().entity(entity).contains::<InteractionDisabled>());
        app.world_mut().trigger(Activate { entity });
        assert!(!app.world().resource::<LauncherState>().requested);
        {
            let mut frame = app.world_mut().resource_mut::<ShellFrameState>();
            let panel = &mut frame.0.panels[Edge::Bottom.index()];
            panel.mapped = true;
            panel.active_page_id = Some("launcher".into());
        }
        app.world_mut().run_system_once(present).unwrap();
        assert!(!app.world().entity(entity).contains::<InteractionDisabled>());
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(entity, bevy::input_focus::FocusCause::Navigated);
        app.world_mut().resource_mut::<ShellFrameState>().0.panels[Edge::Bottom.index()]
            .active_page_id = Some("power".into());
        app.world_mut().run_system_once(present).unwrap();
        assert!(app.world().entity(entity).contains::<InteractionDisabled>());
        assert_eq!(app.world().resource::<InputFocus>().get(), None);
        // Even a stale enabled component cannot activate another page.
        app.world_mut()
            .entity_mut(entity)
            .remove::<InteractionDisabled>();
        app.world_mut().trigger(Activate { entity });
        assert!(!app.world().resource::<LauncherState>().requested);
        app.world_mut().resource_mut::<ShellFrameState>().0.panels[Edge::Bottom.index()]
            .active_page_id = Some("launcher".into());
        app.world_mut().trigger(Activate { entity });
        assert!(app.world().resource::<LauncherState>().requested);
        assert!(app.world().resource::<LauncherState>().busy);
        app.world_mut().resource_mut::<LauncherState>().requested = false;
        app.world_mut().trigger(Activate { entity });
        assert!(
            !app.world().resource::<LauncherState>().requested,
            "busy launcher rejects duplicates"
        );
    }
}
