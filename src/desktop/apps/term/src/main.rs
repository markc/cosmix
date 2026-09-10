mod bus;
mod config;
#[cfg(test)]
mod config_precedence_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod layout_tests;
mod metrics;
mod panes;
mod raster;
mod tabs;
mod terminal;

use bevy::{
    asset::RenderAssetUsages,
    feathers::{FeathersPlugins, dark_theme::create_dark_theme, theme::UiTheme},
    image::ImageSampler,
    input::{ButtonState, keyboard::KeyboardInput},
    input_focus::{FocusCause, FocusedInput, InputFocus},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    winit::{EventLoopProxyWrapper, UpdateMode, WinitSettings, WinitUserEvent},
};
use cosmix_app_identity::AppIdentity;
use ctk::prelude::*;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tabs::TabSet;
use terminal::Key as TerminalKey;

/// Event-order state, independent of ButtonInput's end-of-batch snapshot.
#[derive(Resource, Default)]
struct Modifiers([bool; 8]);
impl Modifiers {
    fn update(&mut self, key: KeyCode, state: ButtonState) {
        let index = match key {
            KeyCode::ControlLeft => 0,
            KeyCode::ControlRight => 1,
            KeyCode::ShiftLeft => 2,
            KeyCode::ShiftRight => 3,
            KeyCode::AltLeft => 4,
            KeyCode::AltRight => 5,
            KeyCode::SuperLeft => 6,
            KeyCode::SuperRight => 7,
            _ => return,
        };
        self.0[index] = state == ButtonState::Pressed;
    }
    fn ctrl(&self) -> bool {
        self.0[0] || self.0[1]
    }
    fn shift(&self) -> bool {
        self.0[2] || self.0[3]
    }
    fn alt_or_super(&self) -> bool {
        self.0[4..].iter().any(|held| *held)
    }
}

fn reset_modifiers(
    mut lost: MessageReader<bevy::input::keyboard::KeyboardFocusLost>,
    mut modifiers: ResMut<Modifiers>,
) {
    if lost.read().count() > 0 {
        *modifiers = Modifiers::default();
    }
}

fn control_letter(input: &KeyboardInput) -> Option<char> {
    let text = match &input.logical_key {
        bevy::input::keyboard::Key::Character(text) => Some(text.as_str()),
        _ => input.text.as_deref(),
    }?;
    let mut chars = text.chars();
    let c = chars.next()?;
    (c.is_ascii_alphabetic() && chars.next().is_none()).then_some(c)
}

#[derive(Resource)]
struct Core(Arc<Mutex<TabSet>>, tabs::Cleanup);
/// Sender for completion notes to the Bus task. `None` when TERM_NOTIFY=0
/// disables notifications; sends are best-effort (a full/closed channel is
/// ignored) so notification plumbing can never stall the render thread.
#[derive(Resource)]
struct NotifyTx(Option<tokio::sync::mpsc::UnboundedSender<tabs::CompletionNote>>);
#[derive(Resource)]
struct Painter(Mutex<raster::Raster>);
#[derive(Resource)]
struct View {
    terminal: Entity,
    pane_views: Vec<PaneView>,
    pane_root: Option<Entity>,
    tree_state: Option<(u64, u64)>,
    centre: Entity,
    menu: Entity,
    dropdowns: Vec<(Entity, Vec<(Entity, &'static str)>)>,
    menu_ids: Vec<Vec<&'static str>>,
    menu_item: usize,
    tab_bar: Entity,
    tab_buttons: Vec<Entity>,
    tab_state: Vec<(u64, bool, String)>,

    open_menu: Option<usize>,

    /// Device-pixel scale the Raster is currently built for; refresh rebuilds
    /// it (and forces a re-render) when the window's fractional scale changes.
    scale: f32,
    last_frame: Instant,
}

struct PaneView {
    id: u64,
    container: Entity,
    entity: Entity,
    image: Handle<Image>,
    cols: u16,
    rows: u16,
    rendered: bool,
    active: bool,
}

// VERIFY: precedence — the only font override resolution, reused at every scale.
fn resolve_config(
    mut config: config::Config,
    env_font: Option<&str>,
    term: &'static str,
) -> config::Settings {
    if let Some(px) = env_font
        .and_then(|value| value.parse().ok())
        .filter(|px| config::valid_font(*px))
    {
        config.font_px = px;
    }
    config::Settings { config, term }
}

fn main() {
    let identity = AppIdentity {
        slug: "term",
        display_name: "CosMix Term",
    };
    assert!(identity.validate().is_ok());
    if std::env::args().any(|arg| arg == "--help") {
        println!(
            "{}\nFont: TERM_SPIKE_FONT=/path/to/font.ttf\n--print-config: print resolved startup settings and exit",
            bus::HELP
        );
        return;
    }
    let path = config::config_path(
        std::env::var_os("XDG_CONFIG_HOME").map(Into::into),
        std::env::var_os("HOME").map(Into::into),
    );
    let settings = resolve_config(
        config::load(path.as_deref()),
        std::env::var("TERM_FONT_PX").ok().as_deref(),
        config::selected_term(),
    );
    // VERIFY: print-config — no Wayland, font, PTY or Bus initialisation.
    if std::env::args().any(|arg| arg == "--print-config") {
        println!(
            "{}",
            serde_json::to_string_pretty(&settings).expect("validated config")
        );
        return;
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("term requires a native Wayland session");
        std::process::exit(1);
    }
    // Start at scale 1.0; `refresh` rebuilds the Raster at the window's real
    // fractional scale once the surface is configured, so text is rasterised
    // at physical resolution and never upscaled (the HiDPI blur fix).
    let painter = raster::Raster::new(1.0, settings.config.font_px, settings.config.cursor)
        .unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1)
        });
    let terminal = Arc::new(Mutex::new(TabSet::with_settings(settings).unwrap_or_else(
        |e| {
            eprintln!("PTY startup: {e}");
            std::process::exit(1)
        },
    )));
    let (cleanup, reaper) = tabs::Cleanup::start().expect("terminal cleanup worker");
    // Completion notifications: the reap system (render thread) hands
    // self-exited pane identities to the Bus task, which emits interact.notify.
    // TERM_NOTIFY=0 disables it — the sender is dropped, so notes are never
    // queued and the Bus task retires its receive branch on the first close.
    let notify_enabled = std::env::var("TERM_NOTIFY").map(|value| value != "0").unwrap_or(true);
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
    let bus = bus::start(terminal.clone(), cleanup.clone(), notify_rx);
    App::new()
        .insert_resource(settings)
        .insert_resource(Core(terminal.clone(), cleanup.clone()))
        .insert_resource(NotifyTx(notify_enabled.then_some(notify_tx)))
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
        .init_resource::<Modifiers>()
        .add_systems(
            PreUpdate,
            reset_modifiers.before(bevy::input_focus::InputFocusSystems::Dispatch),
        )
        // Focus-loss and key messages have separate streams. Clear both before
        // routing and after dispatch so an old press in that batch cannot latch.
        .add_systems(
            PreUpdate,
            reset_modifiers.after(bevy::input_focus::InputFocusSystems::Dispatch),
        )
        .add_observer(keyboard)
        .add_observer(on_menu)
        .add_systems(Update, (menu_focus, sync_tabs, sync_panes).chain())
        .add_systems(PostUpdate, refresh.after(bevy::ui::UiSystems::Layout))
        .run();
    let removed = terminal.lock().unwrap().shutdown();
    cleanup.submit(removed);
    // Let a last-tab Bus close finish its bounded reply before process exit.
    let _ = bus.join();
    drop(cleanup);
    let _ = reaper.join();
}
fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    core: Res<Core>,
    proxy: Res<EventLoopProxyWrapper>,
    mut focus: ResMut<InputFocus>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);
    let menus = [
        MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new("tab.new", "New Tab"),
                MenuItemDef::new("tab.close", "Close Tab"),
                MenuItemDef::new("app.quit", "Quit"),
            ],
        },
        MenuDef {
            label: "Help".into(),
            items: vec![MenuItemDef::new("help.about", "About")],
        },
    ];
    let menu_ids = menus
        .iter()
        .map(|menu| menu.items.iter().map(|item| item.id).collect())
        .collect();
    let menu = spawn_menu_bar(&mut commands, &menus);
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
        .id();
    let tab_bar = commands
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            flex_shrink: 0.0,
            column_gap: px(4),
            ..default()
        })
        .id();
    commands
        .spawn(Node {
            width: percent(100),
            height: percent(100),
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_children(&[menu, tab_bar, centre]);
    focus.set(centre, FocusCause::Navigated);
    commands.insert_resource(View {
        terminal: centre,
        pane_views: Vec::new(),
        pane_root: None,
        tree_state: None,
        centre,
        menu,
        dropdowns: Vec::new(),
        menu_ids,
        menu_item: 0,
        tab_bar,
        tab_buttons: Vec::new(),
        tab_state: Vec::new(),
        open_menu: None,
        scale: 1.0,
        last_frame: Instant::now(),
    });
    let proxy = (**proxy).clone();
    core.0.lock().unwrap().set_wake(Arc::new(move || {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    }));
}
fn sync_tabs(mut commands: Commands, core: Res<Core>, mut view: ResMut<View>) {
    let tabs = core.0.lock().unwrap().list();
    let state: Vec<_> = tabs
        .iter()
        .map(|tab| (tab.id, tab.active, tab.title.clone()))
        .collect();
    if state == view.tab_state {
        return;
    }
    for button in view.tab_buttons.drain(..) {
        commands.entity(button).despawn();
    }
    view.tab_state = state;
    for tab in tabs {
        let id = tab.id;
        let button = ctk::button::spawn_button(
            &mut commands,
            ButtonDef::text(tab.title).variant(if tab.active {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Default
            }),
        );
        commands.entity(button).observe(
            move |_: On<bevy::ui_widgets::Activate>,
                  core: Res<Core>,
                  view: Res<View>,
                  mut focus: ResMut<InputFocus>| {
                core.0.lock().unwrap().select(id);
                focus.set(view.terminal, FocusCause::Pressed);
            },
        );
        commands.entity(view.tab_bar).add_child(button);
        view.tab_buttons.push(button);
    }
    let button = ctk::button::spawn_button(&mut commands, ButtonDef::text("+"));
    commands.entity(button).observe(
        |_: On<bevy::ui_widgets::Activate>,
         core: Res<Core>,
         view: Res<View>,
         mut focus: ResMut<InputFocus>| {
            menu_action("tab.new", &core);
            focus.set(view.terminal, FocusCause::Pressed);
        },
    );
    commands.entity(view.tab_bar).add_child(button);
    view.tab_buttons.push(button);
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
                        let ids = view
                            .menu_ids
                            .get(view.dropdowns.len())
                            .cloned()
                            .unwrap_or_default();
                        // Fail closed on structural drift: never guess which child is an action.
                        let entries = if entries.len() == ids.len() {
                            entries.into_iter().zip(ids).collect()
                        } else {
                            eprintln!(
                                "CTK menu entry structure changed; keyboard activation disabled"
                            );
                            Vec::new()
                        };
                        view.dropdowns.push((entity, entries));
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
        view.menu_item = 0;
        focus.set(
            open.and_then(|index| view.dropdowns[index].1.first().map(|entry| entry.0))
                .unwrap_or(view.terminal),
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
        "tab.new" => {
            if let Err(e) = core.0.lock().unwrap().open() {
                eprintln!("new tab: {e}");
            }
        }
        "tab.close" => {
            let mut tabs = core.0.lock().unwrap();
            if !tabs.is_empty() {
                let id = tabs.active_id();
                let removed = tabs.close(id).1;
                drop(tabs);
                core.1.submit(removed.into_iter().collect());
            }
        }
        "app.quit" => {
            let removed = core.0.lock().unwrap().shutdown();
            core.1.submit(removed);
        }
        _ => {}
    }
}
fn keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    mut modifiers: ResMut<Modifiers>,
    core: Res<Core>,
    mut view: ResMut<View>,
    mut nodes: Query<&mut Node>,
    mut focus: ResMut<InputFocus>,
    capture: Res<ModalCapture>,
) {
    modifiers.update(event.input.key_code, event.input.state);
    if event.input.state != ButtonState::Pressed {
        return;
    }
    let ctrl = modifiers.ctrl();
    let shift = modifiers.shift();
    let open = view
        .dropdowns
        .iter()
        .position(|(e, _)| nodes.get(*e).is_ok_and(|n| n.display != Display::None));
    if open.is_none()
        && terminal_focused(&view, event.focused_entity)
        && !capture.is_captured()
        && ctrl
        && !modifiers.alt_or_super()
    {
        let handled = match event.input.key_code {
            KeyCode::KeyT if shift => {
                if !event.input.repeat {
                    menu_action("tab.new", &core);
                }
                true
            }
            KeyCode::KeyW if shift => {
                if !event.input.repeat {
                    menu_action("tab.close", &core);
                }
                true
            }
            KeyCode::KeyE
            | KeyCode::KeyO
            | KeyCode::KeyX
            | KeyCode::ArrowLeft
            | KeyCode::ArrowRight
            | KeyCode::ArrowUp
            | KeyCode::ArrowDown
                if shift =>
            {
                if !event.input.repeat {
                    let mut tabs = core.0.lock().unwrap();
                    if !tabs.is_empty() {
                        let removed = match event.input.key_code {
                            KeyCode::KeyE | KeyCode::KeyO => {
                                let dir = if event.input.key_code == KeyCode::KeyE {
                                    panes::SplitDir::Vertical
                                } else {
                                    panes::SplitDir::Horizontal
                                };
                                if let Err(error) = tabs.split_active(dir) {
                                    eprintln!("split pane: {error}");
                                }
                                None
                            }
                            KeyCode::KeyX => tabs.close_active().1,
                            key => {
                                let dir = match key {
                                    KeyCode::ArrowLeft => panes::Direction::Left,
                                    KeyCode::ArrowRight => panes::Direction::Right,
                                    KeyCode::ArrowUp => panes::Direction::Up,
                                    _ => panes::Direction::Down,
                                };
                                tabs.focus_dir(dir);
                                None
                            }
                        };
                        drop(tabs);
                        core.1.submit(removed.into_iter().collect());
                    }
                }
                true
            }
            KeyCode::PageDown | KeyCode::PageUp if !shift => {
                if !event.input.repeat {
                    core.0
                        .lock()
                        .unwrap()
                        .cycle(event.input.key_code == KeyCode::PageDown);
                }
                true
            }
            _ => false,
        };
        if handled {
            event.propagate(false);
            return;
        }
    }
    if let Some(index) = open {
        // Immutable event target is never replayed after dismissal.
        event.propagate(false);
        match event.input.key_code {
            KeyCode::ArrowDown | KeyCode::ArrowUp | KeyCode::Tab => {
                let count = view.dropdowns[index].1.len();
                if count == 0 {
                    return;
                }
                let backwards = event.input.key_code == KeyCode::ArrowUp
                    || (event.input.key_code == KeyCode::Tab && shift);
                view.menu_item = (view.menu_item + if backwards { count - 1 } else { 1 }) % count;
                focus.set(
                    view.dropdowns[index].1[view.menu_item].0,
                    FocusCause::Navigated,
                );
            }
            KeyCode::Enter | KeyCode::Escape => {
                if event.input.key_code == KeyCode::Enter
                    && let Some((_, id)) = view.dropdowns[index].1.get(view.menu_item)
                {
                    menu_action(id, &core);
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
    if capture.is_captured() || !terminal_focused(&view, event.focused_entity) {
        return;
    }
    if modifiers.alt_or_super() {
        return;
    }
    let key = if ctrl {
        control_letter(&event.input).map(TerminalKey::Control)
    } else {
        match event.input.key_code {
            KeyCode::Escape => Some(TerminalKey::Escape),
            KeyCode::Home => Some(TerminalKey::Home),
            KeyCode::End => Some(TerminalKey::End),
            KeyCode::Delete => Some(TerminalKey::Delete),
            KeyCode::PageUp => Some(TerminalKey::PageUp),
            KeyCode::PageDown => Some(TerminalKey::PageDown),
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
    let tabs = core.0.lock().unwrap();
    if tabs.is_empty() {
        return;
    }
    let active = tabs.active_terminal();
    let terminal = active.lock().unwrap();
    let mut sent = false;
    if let Some(key) = key {
        if let Err(e) = terminal.listener.key(key, at) {
            eprintln!("input: {e}");
        } else {
            sent = true;
        }
    } else if !ctrl && let Some(text) = &event.input.text {
        for c in text.chars().filter(|c| c.is_ascii() && !c.is_control()) {
            if let Err(e) = terminal.listener.key(TerminalKey::Char(c), at) {
                eprintln!("input: {e}");
            } else {
                sent = true;
            }
        }
    }
    if sent {
        event.propagate(false);
    }
}
fn terminal_focused(view: &View, entity: Entity) -> bool {
    entity == view.terminal
        || view
            .pane_views
            .iter()
            .any(|pane| pane.entity == entity || pane.container == entity)
}

fn spawn_pane_tree(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    tree: &panes::PaneTree,
    views: &mut Vec<PaneView>,
) -> Entity {
    use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor};
    use ctk::theme::tokens;
    match tree {
        panes::PaneTree::Leaf(pane) => {
            let id = pane.id;
            let mut placeholder = Image::new_fill(
                Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                &[0, 0, 0, 255],
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::default(),
            );
            placeholder.sampler = ImageSampler::nearest();
            let image = images.add(placeholder);
            let entity = commands
                .spawn((
                    ImageNode::new(image.clone()),
                    Node {
                        flex_shrink: 0.0,
                        ..default()
                    },
                ))
                .id();
            let container = commands
                .spawn((
                    Node {
                        width: percent(100),
                        height: percent(100),
                        min_width: px(0),
                        min_height: px(0),
                        border: UiRect::all(px(1)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                    BorderColor::DEFAULT,
                    ThemeBorderColor(tokens::BORDER),
                ))
                .add_child(entity)
                .observe(
                    move |_: On<Pointer<Click>>,
                          core: Res<Core>,
                          mut view: ResMut<View>,
                          mut focus: ResMut<InputFocus>| {
                        if core.0.lock().unwrap().focus(id) {
                            view.terminal = entity;
                            focus.set(entity, FocusCause::Pressed);
                        }
                    },
                )
                .id();
            views.push(PaneView {
                id,
                container,
                entity,
                image,
                cols: 80,
                rows: 24,
                rendered: false,
                active: false,
            });
            container
        }
        panes::PaneTree::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let vertical = *dir == panes::SplitDir::Vertical;
            let root = commands
                .spawn(Node {
                    width: percent(100),
                    height: percent(100),
                    min_width: px(0),
                    min_height: px(0),
                    flex_direction: if vertical {
                        FlexDirection::Row
                    } else {
                        FlexDirection::Column
                    },
                    ..default()
                })
                .id();
            // A zero basis divides the space left AFTER the fixed divider.
            // These weighted slots contain the recursively mirrored subtrees.
            for (index, (tree, weight)) in [(first, *ratio), (second, 1.0 - ratio)]
                .into_iter()
                .enumerate()
            {
                if index == 1 {
                    let divider = commands
                        .spawn((
                            Node {
                                width: if vertical { px(3) } else { percent(100) },
                                height: if vertical { percent(100) } else { px(3) },
                                flex_shrink: 0.0,
                                ..default()
                            },
                            ThemeBackgroundColor(tokens::BORDER),
                        ))
                        .id();
                    commands.entity(root).add_child(divider);
                }
                let slot = commands
                    .spawn(Node {
                        flex_basis: px(0),
                        flex_grow: weight,
                        min_width: px(0),
                        min_height: px(0),
                        width: if vertical { Val::Auto } else { percent(100) },
                        height: if vertical { percent(100) } else { Val::Auto },
                        overflow: Overflow::clip(),
                        ..default()
                    })
                    .id();
                let child = spawn_pane_tree(commands, images, tree, views);
                commands.entity(slot).add_child(child);
                commands.entity(root).add_child(slot);
            }
            root
        }
    }
}

fn sync_panes(
    mut commands: Commands,
    core: Res<Core>,
    mut view: ResMut<View>,
    mut images: ResMut<Assets<Image>>,
    mut focus: ResMut<InputFocus>,
    borders: Query<&bevy::feathers::theme::ThemeBorderColor>,
) {
    let tabs = core.0.lock().unwrap();
    if tabs.is_empty() {
        return;
    }
    let state = (tabs.active_id(), tabs.active_tab().revision);
    let restore_focus = focus
        .get()
        .is_some_and(|entity| terminal_focused(&view, entity));
    if view.tree_state != Some(state) {
        if let Some(root) = view.pane_root.take() {
            commands.entity(root).despawn();
        }
        view.pane_views.clear();
        let root = spawn_pane_tree(
            &mut commands,
            &mut images,
            &tabs.active_tab().tree,
            &mut view.pane_views,
        );
        commands.entity(view.centre).add_child(root);
        view.pane_root = Some(root);
        view.tree_state = Some(state);
    }
    let active = tabs.active_tab().active_pane;
    for pane in &view.pane_views {
        let token = if pane.id == active {
            ctk::theme::tokens::CONTROL_ACTIVE
        } else {
            ctk::theme::tokens::BORDER
        };
        // The Bus can change focus after Update but before refresh: compare
        // the installed border token, independently of the last drawn cursor.
        if !borders
            .get(pane.container)
            .is_ok_and(|border| border.0 == token)
        {
            commands
                .entity(pane.container)
                .insert(bevy::feathers::theme::ThemeBorderColor(token));
        }
    }
    if let Some(pane) = view.pane_views.iter().find(|pane| pane.id == active) {
        let entity = pane.entity;
        view.terminal = entity;
        if restore_focus && focus.get() != Some(entity) {
            focus.set(entity, FocusCause::Navigated);
        }
    }
}

/// Layout resolves and rounds each border in physical pixels independently.
fn pane_interior(computed: &ComputedNode) -> Vec2 {
    let border = computed.border();
    let insets = border.min_inset + border.max_inset;
    (computed.size() - insets).max(Vec2::ZERO) * computed.inverse_scale_factor()
}

fn refresh(
    core: Res<Core>,
    notify: Res<NotifyTx>,
    painter: Res<Painter>,
    settings: Res<config::Settings>,
    mut view: ResMut<View>,
    mut images: ResMut<Assets<Image>>,
    mut nodes: Query<(&ComputedNode, &UiGlobalTransform, &mut Node)>,
    mut exit: MessageWriter<AppExit>,
) {
    let (removed, notes) = core.0.lock().unwrap().reap_exited();
    core.1.submit(removed);
    // Hand each self-exited pane to the Bus task for an interact.notify. A
    // dropped/closed channel is ignored — the notification is a courtesy, never
    // a correctness dependency of the reap.
    if let Some(tx) = &notify.0 {
        for note in notes {
            let _ = tx.send(note);
        }
    }
    let mut tabs = core.0.lock().unwrap();
    if tabs.is_empty() {
        exit.write(AppExit::Success);
        return;
    }
    // A Bus mutation or an exit after Update must wait for its new layout.
    if view.tree_state != Some((tabs.active_id(), tabs.active_tab().revision)) {
        return;
    }
    let now = Instant::now();
    let elapsed = now - view.last_frame;
    view.last_frame = now;
    let mut painter = painter.0.lock().unwrap();
    let mut rebuilt = false;
    let Ok((centre, centre_transform, _)) = nodes.get(view.centre) else {
        return;
    };
    let inv = centre.inverse_scale_factor();
    let origin = centre_transform.affine().translation * inv - centre.size() * inv / 2.0;
    let scale = if inv > 0.0 { 1.0 / inv } else { 1.0 };
    if (scale - view.scale).abs() > 0.01 {
        match raster::Raster::new(scale, settings.config.font_px, settings.config.cursor) {
            Ok(next) => {
                *painter = next;
                rebuilt = true;
            }
            Err(e) => eprintln!("raster rebuild at scale {scale}: {e}"),
        }
        view.scale = scale;
    }
    let active_id = tabs.active_tab().active_pane;
    for pane in &mut view.pane_views {
        let Some(terminal) = tabs.pane_by_id(pane.id) else {
            continue;
        };
        let terminal = terminal.lock().unwrap();
        {
            let mut stats = terminal.stats.lock().unwrap();
            stats.frames += 1;
            stats.frame.add(elapsed);
        }
        let active = pane.id == active_id;
        let switched = rebuilt || !pane.rendered || active != pane.active;
        // VERIFY: per-leaf resize+HiDPI node sizing — logical allocation,
        // physical PTY/texture pixels, logical image pixels divided by scale.
        if let Ok((computed, transform, _)) = nodes.get(pane.container) {
            let inv = computed.inverse_scale_factor();
            let outer = computed.size() * inv;
            let position = transform.affine().translation * inv - outer / 2.0 - origin;
            tabs.geometry(
                pane.id,
                panes::Geometry {
                    x: position.x,
                    y: position.y,
                    w: outer.x,
                    h: outer.y,
                },
            );
            let size = pane_interior(computed);
            let cols = ((size.x / painter.logical_width().max(1.0)) as u16)
                .clamp(2, 240.min((4096 / painter.width) as u16));
            let rows = ((size.y / painter.logical_height().max(1.0)) as u16)
                .clamp(1, 100.min((4096 / painter.height) as u16));
            if size.x > 0.0 && size.y > 0.0 && (switched || (cols, rows) != (pane.cols, pane.rows))
            {
                terminal.resize(
                    cols,
                    rows,
                    cols * painter.width as u16,
                    rows * painter.height as u16,
                );
                tabs.resized(pane.id, cols, rows);
                pane.cols = cols;
                pane.rows = rows;
            }
        }
        let damaged = terminal.take_damage();
        if !switched && !damaged {
            continue;
        }
        // VERIFY: per-leaf render — all visible leaves consume their own damage.
        let mut screen = terminal.screen(true);
        screen.cursor_visible &= active;
        pane.active = active;
        pane.rendered = true;
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
        if let Some(mut image) = images.get_mut(&pane.image) {
            let mut next = Image::new(
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
            next.sampler = ImageSampler::nearest();
            *image = next;
            let mut stats = terminal.stats.lock().unwrap();
            stats.rgba_upload.add(converted.elapsed());
            stats.uploads += 1;
        }
        let node_w = px(width as f32 / painter.scale);
        let node_h = px(height as f32 / painter.scale);
        if let Ok((_, _, mut node)) = nodes.get_mut(pane.entity)
            && (node.width != node_w || node.height != node_h)
        {
            node.width = node_w;
            node.height = node_h;
            terminal.listener.wake();
        }
    }
}
