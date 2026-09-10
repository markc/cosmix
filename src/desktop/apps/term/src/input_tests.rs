use super::*;
use bevy::input::keyboard::{Key as LogicalKey, KeyboardFocusLost, NativeKey};
use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin, InputFocusSystems};
use bevy::window::PrimaryWindow;

fn fixture() -> Option<(App, Entity)> {
    if !std::path::Path::new("/opt/cosmix/bin/mix").is_file() {
        eprintln!("SKIP input PTY test: Mix unavailable");
        return None;
    }
    let mut app = App::new();
    app.add_plugins((
        bevy::input::InputPlugin,
        InputFocusPlugin,
        InputDispatchPlugin,
    ))
    .init_resource::<Modifiers>()
    .init_resource::<ModalCapture>()
    .add_observer(keyboard)
    .add_systems(
        PreUpdate,
        reset_modifiers.before(InputFocusSystems::Dispatch),
    )
    .add_systems(
        PreUpdate,
        reset_modifiers.after(InputFocusSystems::Dispatch),
    );
    let window = app
        .world_mut()
        .spawn((Window::default(), PrimaryWindow))
        .id();
    let terminal = app
        .world_mut()
        .spawn((Node::default(), ChildOf(window)))
        .id();
    let menu = app.world_mut().spawn(Node::default()).id();
    let dropdown = app
        .world_mut()
        .spawn(Node {
            display: Display::None,
            ..default()
        })
        .id();
    let entry = app.world_mut().spawn_empty().id();
    let (cleanup, _) = tabs::Cleanup::start().unwrap();
    app.insert_resource(Core(Arc::new(Mutex::new(TabSet::new().unwrap())), cleanup));
    app.insert_resource(View {
        pane_views: vec![],
        pane_root: None,
        tree_state: None,
        terminal,
        centre: terminal,
        menu,
        dropdowns: vec![(dropdown, vec![(entry, "tab.new")])],
        menu_ids: vec![vec!["tab.new"]],
        menu_item: 0,
        tab_bar: menu,
        tab_buttons: vec![],
        tab_state: vec![],
        open_menu: None,
        scale: 1.0,
        last_frame: Instant::now(),
    });
    app.world_mut()
        .resource_mut::<InputFocus>()
        .set(terminal, FocusCause::Navigated);
    Some((app, window))
}

fn input(app: &mut App, window: Entity, code: KeyCode, state: ButtonState, repeat: bool) {
    let text = match code {
        KeyCode::KeyT => Some("t"),
        KeyCode::KeyW => Some("w"),
        _ => None,
    };
    app.world_mut().write_message(KeyboardInput {
        key_code: code,
        logical_key: text.map_or(LogicalKey::Unidentified(NativeKey::Unidentified), |s| {
            LogicalKey::Character(s.into())
        }),
        state,
        text: text.map(Into::into),
        repeat,
        window,
    });
}
fn press(app: &mut App, window: Entity, code: KeyCode) {
    input(app, window, code, ButtonState::Pressed, false);
}
fn count(app: &App) -> usize {
    app.world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .list()
        .len()
}
fn chord(app: &mut App, window: Entity, code: KeyCode) {
    press(app, window, KeyCode::ControlLeft);
    press(app, window, KeyCode::ShiftLeft);
    press(app, window, code);
}

#[test]
fn shortcut_uses_event_order_with_releases_in_same_batch() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    chord(&mut app, window, KeyCode::KeyT);
    input(
        &mut app,
        window,
        KeyCode::ShiftLeft,
        ButtonState::Released,
        false,
    );
    input(
        &mut app,
        window,
        KeyCode::ControlLeft,
        ButtonState::Released,
        false,
    );
    app.update();
    assert_eq!(count(&app), 2);
    assert!(!app.world().resource::<Modifiers>().ctrl());
    assert!(!app.world().resource::<Modifiers>().shift());
}

#[test]
fn later_modifier_presses_do_not_turn_plain_key_into_shortcut() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    press(&mut app, window, KeyCode::KeyT);
    press(&mut app, window, KeyCode::ControlLeft);
    press(&mut app, window, KeyCode::ShiftLeft);
    app.update();
    assert_eq!(count(&app), 1);
}

#[test]
fn shortcuts_require_closed_menu_terminal_focus_and_no_capture() {
    for blocked in 0..3 {
        let Some((mut app, window)) = fixture() else {
            return;
        };
        match blocked {
            0 => {
                let dropdown = app.world().resource::<View>().dropdowns[0].0;
                app.world_mut().get_mut::<Node>(dropdown).unwrap().display = Display::Flex;
            }
            1 => {
                app.world_mut()
                    .resource_mut::<InputFocus>()
                    .set(window, FocusCause::Navigated);
            }
            _ => {
                app.world_mut().resource_mut::<ModalCapture>().acquire(
                    ctk::modal_capture::ModalCaptureOwner {
                        kind: "test",
                        entity: None,
                    },
                    ctk::modal_capture::ModalCaptureLayer(1),
                );
            }
        }
        chord(&mut app, window, KeyCode::KeyW);
        app.update();
        assert_eq!(count(&app), 1, "blocked case {blocked}");
    }
}

#[test]
fn shortcuts_reject_extra_modifiers_and_repeat() {
    for extra in [KeyCode::AltLeft, KeyCode::SuperRight] {
        let Some((mut app, window)) = fixture() else {
            return;
        };
        press(&mut app, window, extra);
        chord(&mut app, window, KeyCode::KeyT);
        app.update();
        assert_eq!(count(&app), 1);
    }
    let Some((mut app, window)) = fixture() else {
        return;
    };
    let first = app.world().resource::<Core>().0.lock().unwrap().active_id();
    app.world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .open()
        .unwrap();
    let second = app.world().resource::<Core>().0.lock().unwrap().active_id();
    press(&mut app, window, KeyCode::ControlLeft);
    input(
        &mut app,
        window,
        KeyCode::PageUp,
        ButtonState::Pressed,
        true,
    );
    app.update();
    assert_eq!(
        app.world().resource::<Core>().0.lock().unwrap().active_id(),
        second
    );
    press(&mut app, window, KeyCode::PageUp);
    app.update();
    assert_eq!(
        app.world().resource::<Core>().0.lock().unwrap().active_id(),
        first
    );
    press(&mut app, window, KeyCode::ShiftLeft);
    press(&mut app, window, KeyCode::PageDown);
    input(&mut app, window, KeyCode::KeyT, ButtonState::Pressed, true);
    app.update();
    assert_eq!(count(&app), 2);
    assert_eq!(
        app.world().resource::<Core>().0.lock().unwrap().active_id(),
        first
    );
}

#[test]
fn focus_loss_clears_event_order_modifiers() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    press(&mut app, window, KeyCode::ControlLeft);
    press(&mut app, window, KeyCode::ShiftLeft);
    app.update();
    app.world_mut().write_message(KeyboardFocusLost);
    app.update();
    press(&mut app, window, KeyCode::KeyT);
    app.update();
    assert_eq!(count(&app), 1);
}

#[test]
fn focus_loss_with_modifier_press_in_same_batch_does_not_latch() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    press(&mut app, window, KeyCode::ControlLeft);
    press(&mut app, window, KeyCode::ShiftLeft);
    app.world_mut().write_message(KeyboardFocusLost);
    app.update();
    press(&mut app, window, KeyCode::KeyT);
    app.update();
    assert_eq!(count(&app), 1);
}

#[test]
fn releasing_one_modifier_side_keeps_other_side_held() {
    let mut modifiers = Modifiers::default();
    modifiers.update(KeyCode::ControlLeft, ButtonState::Pressed);
    modifiers.update(KeyCode::ControlRight, ButtonState::Pressed);
    modifiers.update(KeyCode::ControlLeft, ButtonState::Released);
    assert!(modifiers.ctrl());
    modifiers.update(KeyCode::ControlRight, ButtonState::Released);
    assert!(!modifiers.ctrl());
}

#[test]
fn menu_activation_is_bounds_safe() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    let dropdown = app.world().resource::<View>().dropdowns[0].0;
    app.world_mut().get_mut::<Node>(dropdown).unwrap().display = Display::Flex;
    app.world_mut().resource_mut::<View>().menu_item = usize::MAX;
    press(&mut app, window, KeyCode::Enter);
    app.update();
    assert_eq!(count(&app), 1);
}

#[derive(Resource, Default)]
struct Bubbled(Vec<KeyCode>);

#[test]
fn pane_subtree_is_stable_on_focus_and_repairs_late_bus_border() {
    let Some((mut app, _)) = fixture() else {
        return;
    };
    app.init_resource::<Assets<Image>>()
        .add_systems(Update, sync_panes);
    app.update();
    let original_root = app.world().resource::<View>().pane_root.unwrap();
    let first = app
        .world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .active_tab()
        .active_pane;
    let second = app
        .world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .split_active(panes::SplitDir::Vertical)
        .unwrap();
    app.update();
    let split_root = app.world().resource::<View>().pane_root.unwrap();
    assert_ne!(original_root, split_root);
    assert!(app.world().get_entity(original_root).is_err());
    assert_eq!(
        app.world().get::<Node>(split_root).unwrap().flex_direction,
        FlexDirection::Row
    );
    let views = &app.world().resource::<View>().pane_views;
    assert_eq!(views.len(), 2);
    assert_ne!(views[0].image.id(), views[1].image.id());
    let first_container = views[0].container;
    let second_container = views[1].container;
    app.world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .focus(first);
    // Simulate refresh seeing the Bus focus before sync_panes sees it.
    for pane in &mut app.world_mut().resource_mut::<View>().pane_views {
        pane.rendered = true;
        pane.active = pane.id == first;
    }
    app.update();
    assert_eq!(app.world().resource::<View>().pane_root, Some(split_root));
    assert_eq!(
        app.world()
            .get::<bevy::feathers::theme::ThemeBorderColor>(first_container)
            .unwrap()
            .0,
        ctk::theme::tokens::CONTROL_ACTIVE
    );
    assert_eq!(
        app.world()
            .get::<bevy::feathers::theme::ThemeBorderColor>(second_container)
            .unwrap()
            .0,
        ctk::theme::tokens::BORDER
    );
    app.world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .focus(second);
    app.update();
    assert_eq!(app.world().resource::<View>().pane_root, Some(split_root));
    let removed = app
        .world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .close_active()
        .1;
    drop(removed);
    app.update();
    assert_ne!(app.world().resource::<View>().pane_root, Some(split_root));
    assert_eq!(app.world().resource::<View>().pane_views.len(), 1);
}

#[test]
fn pane_shortcuts_split_focus_close_and_consume_repeats() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    app.init_resource::<Bubbled>();
    app.world_mut().entity_mut(window).observe(
        |event: On<FocusedInput<KeyboardInput>>, mut seen: ResMut<Bubbled>| {
            seen.0.push(event.input.key_code);
        },
    );
    let first = app
        .world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .active_tab()
        .active_pane;
    chord(&mut app, window, KeyCode::KeyE);
    app.update();
    let second = app
        .world()
        .resource::<Core>()
        .0
        .lock()
        .unwrap()
        .active_tab()
        .active_pane;
    assert_ne!(first, second);
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .leaves()
            .len(),
        2
    );
    for key in [
        KeyCode::KeyE,
        KeyCode::KeyO,
        KeyCode::KeyX,
        KeyCode::ArrowLeft,
    ] {
        input(&mut app, window, key, ButtonState::Pressed, true);
    }
    app.update();
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .leaves()
            .len(),
        2
    );
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .active_tab()
            .active_pane,
        second
    );
    press(&mut app, window, KeyCode::ArrowLeft);
    app.update();
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .active_tab()
            .active_pane,
        first
    );
    press(&mut app, window, KeyCode::KeyO);
    app.update();
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .leaves()
            .len(),
        3
    );
    press(&mut app, window, KeyCode::KeyX);
    app.update();
    assert_eq!(
        app.world()
            .resource::<Core>()
            .0
            .lock()
            .unwrap()
            .leaves()
            .len(),
        2
    );
    let seen = &app.world().resource::<Bubbled>().0;
    for key in [
        KeyCode::KeyE,
        KeyCode::KeyO,
        KeyCode::KeyX,
        KeyCode::ArrowLeft,
    ] {
        assert!(!seen.contains(&key), "pane shortcut leaked: {key:?}");
    }
}

#[test]
fn pane_shortcuts_require_menu_focus_and_exact_modifiers() {
    for blocked in 0..6 {
        let Some((mut app, window)) = fixture() else {
            return;
        };
        match blocked {
            0 => {
                let dropdown = app.world().resource::<View>().dropdowns[0].0;
                app.world_mut().get_mut::<Node>(dropdown).unwrap().display = Display::Flex;
            }
            1 => {
                app.world_mut()
                    .resource_mut::<InputFocus>()
                    .set(window, FocusCause::Navigated);
            }
            2 => {
                app.world_mut().resource_mut::<ModalCapture>().acquire(
                    ctk::modal_capture::ModalCaptureOwner {
                        kind: "test",
                        entity: None,
                    },
                    ctk::modal_capture::ModalCaptureLayer(1),
                );
            }
            3 => press(&mut app, window, KeyCode::AltLeft),
            4 => press(&mut app, window, KeyCode::SuperLeft),
            _ => {}
        }
        if blocked == 5 {
            press(&mut app, window, KeyCode::ControlLeft);
            press(&mut app, window, KeyCode::KeyE);
        } else {
            chord(&mut app, window, KeyCode::KeyE);
        }
        app.update();
        assert_eq!(
            app.world()
                .resource::<Core>()
                .0
                .lock()
                .unwrap()
                .leaves()
                .len(),
            1,
            "blocked case {blocked}"
        );
    }
}

#[test]
fn unmapped_control_key_propagates_but_encoded_letter_does_not() {
    let Some((mut app, window)) = fixture() else {
        return;
    };
    app.init_resource::<Bubbled>();
    app.world_mut().entity_mut(window).observe(
        |event: On<FocusedInput<KeyboardInput>>, mut seen: ResMut<Bubbled>| {
            seen.0.push(event.input.key_code);
        },
    );
    press(&mut app, window, KeyCode::ControlLeft);
    press(&mut app, window, KeyCode::F1);
    press(&mut app, window, KeyCode::KeyW);
    app.update();
    let seen = &app.world().resource::<Bubbled>().0;
    assert!(seen.contains(&KeyCode::F1), "bubbled keys: {seen:?}");
    assert!(!seen.contains(&KeyCode::KeyW), "bubbled keys: {seen:?}");
    assert_eq!(count(&app), 1);
}
