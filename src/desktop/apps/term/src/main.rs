mod bus;
mod metrics;
mod raster;
mod terminal;

use bevy::{
    asset::RenderAssetUsages,
    feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme},
    input::{ButtonState, keyboard::KeyboardInput},
    input_focus::{FocusCause, FocusedInput, InputFocus},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    winit::{EventLoopProxyWrapper, UpdateMode, WinitSettings, WinitUserEvent},
};
use cosmix_app_identity::AppIdentity;
use ctk::prelude::*;
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use terminal::{Key as TerminalKey, Terminal};

#[derive(Resource)]
struct Core(Arc<Mutex<Terminal>>);
#[derive(Resource)]
struct Painter(Mutex<raster::Raster>);
#[derive(Resource)]
struct View {
    image: Handle<Image>,
    terminal: Entity,
    centre: Entity,
    menu: Entity,
    dropdowns: Vec<(Entity, Entity)>,
    open_menu: Option<usize>,
    cols: u16,
    rows: u16,
    last_frame: Instant,
}

fn main() {
    let identity = AppIdentity {
        slug: "term",
        display_name: "CosMix Term",
    };
    assert!(identity.validate().is_ok());
    if std::env::args().any(|arg| arg == "--help") {
        println!("{}\nFont: TERM_SPIKE_FONT=/path/to/font.ttf", bus::HELP);
        return;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("term requires a native Wayland session");
        std::process::exit(1);
    }
    let painter = raster::Raster::new().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    // The preserved core launches Mix in HOME with TERM=xterm-256color.
    // Product terminfo xterm-rio is P2.2.
    let terminal = Arc::new(Mutex::new(Terminal::start().unwrap_or_else(|e| {
        eprintln!("PTY startup: {e}");
        std::process::exit(1)
    })));
    bus::start(terminal.clone());
    App::new()
        .insert_resource(Core(terminal.clone()))
        .insert_resource(Painter(Mutex::new(painter)))
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: identity.display_name.into(),
                name: Some(identity.app_id()),
                resolution: (900, 560).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkWidgetsPlugin,
            MenuBarPlugin,
        ))
        .insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(16)),
            unfocused_mode: UpdateMode::reactive_low_power(Duration::from_millis(33)),
        })
        .add_systems(Startup, setup)
        .add_systems(PostStartup, bind_menu)
        .add_observer(keyboard)
        .add_observer(on_menu)
        .add_systems(Update, menu_focus)
        .add_systems(PostUpdate, refresh.after(bevy::ui::UiSystems::Layout))
        .run();
    terminal.lock().unwrap().shutdown();
}
fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut images: ResMut<Assets<Image>>,
    core: Res<Core>,
    proxy: Res<EventLoopProxyWrapper>,
    mut focus: ResMut<InputFocus>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);
    let menu = spawn_menu_bar(
        &mut commands,
        &[
            MenuDef {
                label: "File".into(),
                items: vec![MenuItemDef::new("app.quit", "Quit")],
            },
            MenuDef {
                label: "Help".into(),
                items: vec![MenuItemDef::new("help.about", "About")],
            },
        ],
    );
    let image = images.add(Image::new_fill(
        Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    ));
    let terminal = commands
        .spawn((
            ImageNode::new(image.clone()),
            Node {
                flex_shrink: 0.0,
                ..default()
            },
        ))
        .observe(|click: On<Pointer<Click>>, mut focus: ResMut<InputFocus>| {
            focus.set(click.entity, FocusCause::Pressed);
        })
        .id();
    let centre = commands
        .spawn((
            Node {
                width: percent(100),
                flex_grow: 1.0,
                flex_basis: px(0),
                min_height: px(0),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(Color::BLACK),
        ))
        .add_child(terminal)
        .id();
    commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_children(&[menu, centre]);
    focus.set(terminal, FocusCause::Navigated);
    commands.insert_resource(View {
        image,
        terminal,
        centre,
        menu,
        dropdowns: Vec::new(),
        open_menu: None,
        cols: 80,
        rows: 24,
        last_frame: Instant::now(),
    });
    let proxy = (**proxy).clone();
    core.0.lock().unwrap().set_wake(Arc::new(move || {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }));
}
fn bind_menu(mut view: ResMut<View>, children: Query<&Children>, nodes: Query<&Node>) {
    // CTK exposes no bar open-state API. Isolate its current public Node tree
    // adapter here: bar -> anchor -> absolute dropdown. Fail closed on drift.
    if let Ok(anchors) = children.get(view.menu) {
        for anchor in anchors.iter() {
            if let Ok(items) = children.get(anchor) {
                for entity in items.iter() {
                    if nodes
                        .get(entity)
                        .is_ok_and(|n| n.position_type == PositionType::Absolute)
                    {
                        let entries: Vec<_> = children
                            .get(entity)
                            .expect("CTK menu entries")
                            .iter()
                            .collect();
                        assert_eq!(entries.len(), 1, "CTK menu entry structure changed");
                        view.dropdowns.push((entity, entries[0]));
                    }
                }
            }
        }
    }
    assert!(
        view.dropdowns.len() == 2,
        "CTK menu structure changed: cannot establish input isolation"
    );
}
fn menu_focus(mut view: ResMut<View>, nodes: Query<&Node>, mut focus: ResMut<InputFocus>) {
    let open = view
        .dropdowns
        .iter()
        .position(|(e, _)| nodes.get(*e).is_ok_and(|n| n.display != Display::None));
    if open != view.open_menu {
        view.open_menu = open;
        focus.set(
            open.map_or(view.terminal, |index| view.dropdowns[index].1),
            FocusCause::Navigated,
        );
    }
}
fn on_menu(event: On<MenuActivated>, core: Res<Core>) {
    menu_action(event.id, &core);
}
fn menu_action(id: &str, core: &Core) {
    match id {
        "help.about" => println!(
            "CosMix Term · component=term · version={}",
            env!("CARGO_PKG_VERSION")
        ),
        "app.quit" => {
            let core = core.0.lock().unwrap();
            core.listener.quit.store(true, Ordering::Release);
            core.listener.wake();
        }
        _ => {}
    }
}
fn keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    keys: Res<ButtonInput<KeyCode>>,
    core: Res<Core>,
    mut view: ResMut<View>,
    mut nodes: Query<&mut Node>,
    mut focus: ResMut<InputFocus>,
    capture: Res<ModalCapture>,
) {
    if event.input.state != ButtonState::Pressed {
        return;
    }
    let open = view
        .dropdowns
        .iter()
        .position(|(e, _)| nodes.get(*e).is_ok_and(|n| n.display != Display::None));
    if let Some(index) = open {
        // Immutable event target is never replayed after dismissal.
        event.propagate(false);
        match event.input.key_code {
            KeyCode::ArrowDown | KeyCode::ArrowUp | KeyCode::Tab => {
                focus.set(view.dropdowns[index].1, FocusCause::Navigated);
            }
            KeyCode::Enter | KeyCode::Escape => {
                if event.input.key_code == KeyCode::Enter {
                    menu_action(["app.quit", "help.about"][index], &core);
                }
                if let Ok(mut node) = nodes.get_mut(view.dropdowns[index].0) {
                    node.display = Display::None;
                }
                focus.set(view.terminal, FocusCause::Navigated);
                view.open_menu = None;
            }
            _ => {}
        }
        return;
    }
    if capture.is_captured() || event.focused_entity != view.terminal {
        return;
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    if keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight) {
        return;
    }
    let key = if ctrl {
        match event.input.key_code {
            KeyCode::KeyC => Some(TerminalKey::Interrupt),
            KeyCode::KeyD => Some(TerminalKey::Eof),
            _ => None,
        }
    } else {
        match event.input.key_code {
            KeyCode::Enter => Some(TerminalKey::Enter),
            KeyCode::Backspace => Some(TerminalKey::Backspace),
            KeyCode::Tab => Some(TerminalKey::Tab),
            KeyCode::ArrowUp => Some(TerminalKey::Up),
            KeyCode::ArrowDown => Some(TerminalKey::Down),
            KeyCode::ArrowLeft => Some(TerminalKey::Left),
            KeyCode::ArrowRight => Some(TerminalKey::Right),
            _ => None,
        }
    };
    let at = Instant::now();
    let terminal = core.0.lock().unwrap();
    if let Some(key) = key {
        if let Err(e) = terminal.listener.key(key, at) {
            eprintln!("input: {e}");
        }
    } else if !ctrl && let Some(text) = &event.input.text {
        for c in text.chars().filter(|c| c.is_ascii() && !c.is_control()) {
            if let Err(e) = terminal.listener.key(TerminalKey::Char(c), at) {
                eprintln!("input: {e}");
            }
        }
    }
    event.propagate(false);
}
fn refresh(
    core: Res<Core>,
    painter: Res<Painter>,
    mut view: ResMut<View>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<(&ComputedNode, &mut Node)>,
    mut exit: MessageWriter<AppExit>,
) {
    let terminal = core.0.lock().unwrap();
    if terminal.listener.quit.load(Ordering::Acquire) {
        exit.write(AppExit::Success);
        return;
    }
    let now = Instant::now();
    {
        let mut stats = terminal.stats.lock().unwrap();
        stats.frames += 1;
        stats.frame.add(now - view.last_frame);
    }
    view.last_frame = now;
    let mut painter = painter.0.lock().unwrap();
    if let Ok((computed, _)) = nodes.get(view.centre) {
        let size = computed.size() * computed.inverse_scale_factor();
        let cols = ((size.x / painter.width as f32) as u16)
            .clamp(2, 240.min((4096 / painter.width) as u16));
        let rows = ((size.y / painter.height as f32) as u16)
            .clamp(1, 100.min((4096 / painter.height) as u16));
        if size.x > 0.0 && size.y > 0.0 && (cols, rows) != (view.cols, view.rows) {
            terminal.resize(
                cols,
                rows,
                cols * painter.width as u16,
                rows * painter.height as u16,
            );
            view.cols = cols;
            view.rows = rows;
        }
    }
    if !terminal.take_damage() {
        return;
    }
    let screen = terminal.screen(true);
    let rgba = painter.render(&screen);
    let converted = Instant::now();
    terminal
        .stats
        .lock()
        .unwrap()
        .vt_rgba
        .add(converted - screen.updated);
    let width = screen.cols as u32 * painter.width;
    let height = screen.rows as u32 * painter.height;
    if let Some(mut image) = images.get_mut(&view.image) {
        *image = Image::new(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            rgba,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        let mut stats = terminal.stats.lock().unwrap();
        stats.rgba_upload.add(converted.elapsed());
        stats.uploads += 1;
    }
    if let Ok((_, mut node)) = nodes.get_mut(view.terminal)
        && (node.width != px(width as f32) || node.height != px(height as f32))
    {
        node.width = px(width as f32);
        node.height = px(height as f32);
        // Layout has already run; request the one follow-up frame needed
        // for new image geometry, including initial startup under reactive mode.
        terminal.listener.wake();
    }
}
