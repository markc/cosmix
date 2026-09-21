#[cfg(test)]
mod config_precedence_tests;
#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod layout_tests;
mod mouse_input;

use cosmix_term_core::{
    bus, config, native_session, panes, raster, session_fd, tabs, version::version_request,
};

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
use cosmix_actions::{ActionId, Binding, Keymap};
use cosmix_app_identity::AppIdentity;
use ctk::prelude::*;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use cosmix_term_core::{tabs::TabSet, terminal::Key as TerminalKey};

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
/// The core is Bevy-free, so its startup settings ride in this resource.
#[derive(Resource, Clone, Copy)]
struct TermSettings(config::Settings);
#[derive(Component)]
struct SquareButton;

fn square_buttons(mut nodes: Query<&mut Node, With<SquareButton>>) {
    for mut node in &mut nodes {
        if node.width != node.height
            || node.min_width != node.height
            || node.padding != UiRect::ZERO
        {
            node.width = node.height;
            node.min_width = node.height;
            node.padding = UiRect::ZERO;
        }
    }
}

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
    /// What `Raster::paint` remembers about THIS pane's texture between
    /// frames: grid shape, cell size, the buffer's identity, and the cursor
    /// cell it last inverted. All four decide whether the next paint may be
    /// damage-bounded, and all four were bookkept here by hand until
    /// term-core 0.3.0 — including the two that are easy to get wrong, the
    /// row the cursor LEFT (our own inversion, which the grid never reports
    /// as damage) and a shape change that byte length cannot see, 96x25 and
    /// 80x30 being the same 2400 cells and the same number of bytes.
    ///
    /// It is per-target by construction: panes share one glyph cache through
    /// `Painter` and must not share damage bookkeeping.
    paint: raster::PaintState,
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

/// The ctk plugins term runs on, in ONE place so a test can assert against the
/// real set rather than a hand-copied list that drifts from it.
///
/// `ModalCapturePlugin` is here because the `keyboard` observer reads
/// `Res<ModalCapture>` and nothing else installs the authority that owns it:
/// `CtkWidgetsPlugin` does not, and only ctk's interaction and dnd services
/// call `ensure_modal_capture_plugin`, neither of which term uses. Without it
/// the resource never exists and the first focused key event fails the
/// observer's parameter validation — a deterministic panic on any path that
/// reaches a keystroke.
///
/// Installed as a plugin rather than Option-wrapped at the use site: a plugin
/// inserts at build time, strictly before any schedule can run, so the observer
/// cannot fire into a missing resource. Term has menus, menus capture, and the
/// sole consumer is `capture.is_captured()` — treating absence as "nothing is
/// captured" would be a guess that silently diverges the moment one does.
///
/// `InteractionPlugin` registers the `InteractionRequest` message queue and the
/// modal presenter the Help->About dialog rides. `menu_action` writes an
/// `InteractionRequest` through `MessageWriter`, which panics its host system on
/// parameter validation if the message was never registered — so this belongs in
/// the same build-time set as `ModalCapturePlugin`, for the same reason.
fn ctk_plugins() -> (
    CtkWidgetsPlugin,
    MenuBarPlugin,
    ModalCapturePlugin,
    ctk::interaction::InteractionPlugin,
) {
    // Fully-qualified: `InteractionPlugin` is ambiguous — bevy_picking exports
    // one too, and the bare name resolves arbitrarily between the two globs.
    // Term wants ctk's modal presenter, not the picking plugin.
    (
        CtkWidgetsPlugin,
        MenuBarPlugin,
        ModalCapturePlugin,
        ctk::interaction::InteractionPlugin,
    )
}

/// The ctk app-control port term serves **beside** its verified native-session
/// lane: the generic `app.describe` / `app.quit` surface every ctk app shares
/// (so an agent quits or introspects any app by one name).
///
/// Deliberately a *separate* transport from the verified lane, not a merge:
/// that lane is pane-target-scoped with kernel-attested (SO_PEERCRED) admission,
/// per-capability grants and epoch gates — the right home for the high-stakes
/// pane verbs (execute/type/layout), which stay node-local. App-global
/// lifecycle/discovery verbs don't fit that per-pane model, so they ride the
/// port. `app.describe` is open discovery; `app.quit` accepts a local caller OR
/// an admitted, broker-attested mesh peer (network ARexx — any node quitting
/// any app, gated by attestation, not anonymity). `app.quit` writes `AppExit`,
/// so it takes term's normal window-close teardown path.
fn app_port_plugins(identity: &AppIdentity, noded_url: String) -> (BusBridgePlugin, AppPortPlugin) {
    let service_name = format!("{}-{APP_ENGINE}-{}", identity.slug, std::process::id());
    let mut bridge = BusBridgeConfig::new(service_name, noded_url);
    // build_info!() must expand HERE (the app crate) so the registered
    // provenance carries term's version, not ctk's.
    bridge.provenance = provenance_from_build(cosmix_buildinfo::build_info!());
    (
        BusBridgePlugin::new(bridge),
        AppPortPlugin::new(identity.display_name, identity.slug)
            .about(env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_DESCRIPTION")),
    )
}

/// The Bus name this frontend serves under, and the prefix of its verb
/// namespace (D1, TODO-term 2026-09-21). The Bevy frontend is `bterm`; the
/// global name `term` belongs to the iced+wgpu one, so the two can run at
/// once — which is what T5's A/B weight comparison requires.
/// `cosmix_term_core::bus` takes this as a parameter and hardcodes neither.
const SERVICE: &str = "bterm";

fn main() {
    // `--version` first, before the inherited-fd quarantine, the config read,
    // the Wayland check and every other thing below: Mark's contract
    // (2026-09-21) is that a version query does NOTHING except report the
    // version and the build hash. It must answer identically whether or not a
    // bterm is already running, and whether or not there is a display to open
    // — `bterm --version` over ssh with no WAYLAND_DISPLAY used to be an
    // `exit(1)` with "term requires a native Wayland session".
    if let Some(text) = version_request(
        &std::env::args().collect::<Vec<_>>(),
        cosmix_buildinfo::build_info!(),
    ) {
        println!("{text}");
        return;
    }
    session_fd::quarantine_inherited();
    let identity = AppIdentity {
        slug: SERVICE,
        display_name: "CosMix BTerm",
    };
    assert!(identity.validate().is_ok());
    if std::env::args().any(|arg| arg == "--help") {
        println!(
            "CosMix BTerm: tabbed Wayland Mix terminal (Bevy frontend)\nFont: TERM_SPIKE_FONT=/path/to/font.ttf\nTERM_RASTER_TRACE=1: one stderr line per damaged frame — rows painted, full or partial, and what it cost\n--version: print version and build hash, and nothing else\n--print-config: print resolved startup settings and exit\nBus: serves `{SERVICE}` / `{SERVICE}.*`; the global name `term` belongs to the iced frontend"
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
    let mut native = native_session::Supervisor::start()
        .map_err(|error| {
            eprintln!("term native-session disabled: {error}");
        })
        .ok();
    let terminal = Arc::new(Mutex::new(
        TabSet::with_supervisor(settings, native.as_mut()).unwrap_or_else(|e| {
            eprintln!("PTY startup: {e}");
            std::process::exit(1)
        }),
    ));
    let (cleanup, reaper) = tabs::Cleanup::start().expect("terminal cleanup worker");
    let _control = native
        .as_ref()
        .map(|s| s.handle.install_control(terminal.clone(), cleanup.clone()));
    // Completion notifications: the reap system (render thread) hands
    // self-exited pane identities to the Bus task, which emits interact.notify.
    // TERM_NOTIFY=0 disables it — the sender is dropped, so notes are never
    // queued and the Bus task retires its receive branch on the first close.
    let notify_enabled = std::env::var("TERM_NOTIFY")
        .map(|value| value != "0")
        .unwrap_or(true);
    let (notify_tx, notify_rx) = tokio::sync::mpsc::unbounded_channel();
    let bus = bus::start(SERVICE, terminal.clone(), cleanup.clone(), notify_rx);
    // Display-only hints for CTK's accelerator column; keyboard() dispatches keys.
    let keymap = Keymap {
        defaults: [
            ("tab.new", "Ctrl+Shift+T"),
            ("tab.close", "Ctrl+Shift+W"),
            ("app.quit", "Ctrl+Shift+Q"),
            ("help.about", "F1"),
        ]
        .into_iter()
        .map(|(id, chord)| Binding {
            action: ActionId::from_static(id),
            chord: chord.parse().expect("terminal menu chord must be valid"),
            scope: Default::default(),
            repeat: Default::default(),
            allow_in_editable: false,
        })
        .collect(),
        ..Default::default()
    };
    let mut app = App::new();
    app.insert_resource(TermSettings(settings))
        .insert_resource(MenuKeymap::new(1, keymap))
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
        .insert_resource(ctk::theme::CtkThemeMode(ctk::theme::Mode::Dark))
        .add_plugins((FeathersPlugins, CtkThemePlugin::default()))
        .add_plugins(ctk_plugins());
    // Network-ARexx superpowers are earned by mesh membership, not assumed:
    // install the Bus app-control port only on a configured mesh node
    // (`node.conf.mix` present). On a foreign desktop term runs as a plain
    // Wayland app — no port, no broker dialling at all.
    match configured_noded_url() {
        Some(noded_url) => {
            app.add_plugins(app_port_plugins(&identity, noded_url));
        }
        None => {
            eprintln!("term: no node.conf.mix — running standalone; Bus app-control port disabled");
        }
    }
    app.insert_resource(WinitSettings {
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
        .add_systems(Update, record_focus_activity)
        .add_systems(Update, (menu_focus, sync_tabs, sync_panes).chain())
        // CTK's private update_button_style sets height/min_width in Update.
        // Square afterwards every frame, before layout resolves the new size.
        .add_systems(
            PostUpdate,
            square_buttons.before(bevy::ui::UiSystems::Layout),
        )
        .add_systems(PostUpdate, refresh.after(bevy::ui::UiSystems::Layout))
        .run();
    let removed = terminal.lock().unwrap().shutdown();
    cleanup.submit(removed);
    // Let a last-tab Bus close finish its bounded reply before process exit.
    // Ordering constraint: the reaper's receive loop ends only when the LAST
    // Cleanup sender is gone, and the bus thread owns a clone — so every
    // Cleanup (this one and the bus thread's, via join) must be released
    // before reaper.join(), or it hangs forever.
    let _ = bus.join();
    drop(cleanup);
    let _ = reaper.join();
    drop(native);
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
    apply_theme(
        &mut theme,
        &mut theme_state,
        &ThemeSpec::from_scheme(ctk::theme::Scheme::Ocean, ctk::theme::Mode::Dark),
    );
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
                // Match the window's rounded bottom corners. Bevy clips children
                // rectangularly, so pane borders and images are rounded directly.
                border_radius: BorderRadius::bottom(px(9.0)),
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
                let mut tabs = core.0.lock().unwrap();
                tabs.user_activity();
                tabs.select(id);
                focus.set(view.terminal, FocusCause::Pressed);
            },
        );
        commands.entity(view.tab_bar).add_child(button);
        view.tab_buttons.push(button);
    }
    let button =
        ctk::button::spawn_button(&mut commands, ButtonDef::text("+"));
    commands.entity(button).insert(SquareButton);
    commands.entity(button).observe(
        |_: On<bevy::ui_widgets::Activate>,
         core: Res<Core>,
         view: Res<View>,
         mut focus: ResMut<InputFocus>| {
            core.0.lock().unwrap().user_activity();
            tab_action("tab.new", &core);
            focus.set(view.terminal, FocusCause::Pressed);
        },
    );
    commands.entity(view.tab_bar).add_child(button);
    view.tab_buttons.push(button);
}

fn bind_menu(
    mut commands: Commands,
    mut view: ResMut<View>,
    children: Query<&Children>,
    nodes: Query<&Node>,
    text: Query<(), With<Text>>,
) {
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
                    } else if let Ok(labels) = children.get(entity) {
                        // Menu titles stay white rather than inheriting the
                        // selected palette's tinted ctk.text colour.
                        for label in labels.iter().filter(|label| text.contains(*label)) {
                            commands.entity(label)
                                .remove::<bevy::feathers::theme::ThemeTextColor>()
                                .insert(TextColor(Color::WHITE));
                        }
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
fn on_menu(
    event: On<MenuActivated>,
    core: Res<Core>,
    mut about: MessageWriter<InteractionRequest>,
) {
    menu_action(event.id, &core, &mut about);
}
fn record_focus_activity(mut events: MessageReader<bevy::window::WindowFocused>, core: Res<Core>) {
    if events.read().any(|event| event.focused) {
        core.0.lock().unwrap().user_activity();
    }
}
/// Body of the Help->About dialog. A verb tour rather than a blurb: the point of
/// term is that a pane is a Bus citizen an agent can drive, so the About says so.
const ABOUT_BODY: &str = "\
CosMix Term is a Wayland terminal whose panes are first-class citizens on the \
Cosmix Agent Bus.

Every pane registers over the verified native-session lane, so an AI agent — or a \
plain `mix` script with no agent at all — can drive it remotely: run an expression \
and read back the typed value (term.execute), submit an isolated supervised task \
(term.task.submit), read the rendered screen (term.snapshot), and reshape the \
layout (term.pane.split, term.tab.new). No human at the keyboard required.

Capabilities: execute · input · manage_layout · read_contents · read_state · terminate

Part of Cosmix — an agent-operable computing substrate.
https://github.com/markc/cosmix";

fn menu_action(id: &str, core: &Core, about: &mut MessageWriter<InteractionRequest>) {
    if id == "help.about" {
        core.0.lock().unwrap().user_activity();
        about.write(InteractionRequest::text_view(
            "About CosMix Term",
            format!("Terminal · version {}", env!("CARGO_PKG_VERSION")),
            ABOUT_BODY,
        ));
        return;
    }
    tab_action(id, core);
}

/// The menu actions that mutate tabs, split out so the direct callers (the `+`
/// button, the Ctrl+T/Ctrl+W shortcuts) can invoke them without a
/// `MessageWriter` in scope. Only `help.about` needs the writer.
fn tab_action(id: &str, core: &Core) {
    core.0.lock().unwrap().user_activity();
    match id {
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
// Bevy injects these independent resources/queries as observer parameters.
#[allow(clippy::too_many_arguments)]
fn keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    mut modifiers: ResMut<Modifiers>,
    core: Res<Core>,
    mut view: ResMut<View>,
    mut nodes: Query<&mut Node>,
    mut focus: ResMut<InputFocus>,
    capture: Res<ModalCapture>,
    mut about: MessageWriter<InteractionRequest>,
) {
    modifiers.update(event.input.key_code, event.input.state);
    if event.input.state != ButtonState::Pressed {
        return;
    }
    core.0.lock().unwrap().user_activity();
    let ctrl = modifiers.ctrl();
    let shift = modifiers.shift();
    let open = view
        .dropdowns
        .iter()
        .position(|(e, _)| nodes.get(*e).is_ok_and(|n| n.display != Display::None));
    // Help->About: PLAIN F1 (the Help convention), matching the menu item. No
    // modifier, so it sits outside the Ctrl block below; requiring no modifiers
    // keeps Ctrl+F1 etc. unmapped (they still bubble). Guarded by no-menu-open
    // and not-captured. Routed through menu_action so the menu and this key
    // share one path.
    if open.is_none()
        && !capture.is_captured()
        && event.input.key_code == KeyCode::F1
        && !ctrl
        && !shift
        && !modifiers.alt_or_super()
    {
        if !event.input.repeat {
            menu_action("help.about", &core, &mut about);
        }
        event.propagate(false);
        return;
    }
    if open.is_none()
        && terminal_focused(&view, event.focused_entity)
        && !capture.is_captured()
        && ctrl
        && !modifiers.alt_or_super()
    {
        let handled = match event.input.key_code {
            KeyCode::KeyT if shift => {
                if !event.input.repeat {
                    tab_action("tab.new", &core);
                }
                true
            }
            KeyCode::KeyW if shift => {
                if !event.input.repeat {
                    tab_action("tab.close", &core);
                }
                true
            }
            KeyCode::KeyQ if shift => {
                if !event.input.repeat {
                    tab_action("app.quit", &core);
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
                    menu_action(id, &core, &mut about);
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
    active: u64,
) -> Entity {
    use bevy::feathers::theme::ThemeBorderColor;
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
                        border_radius: BorderRadius::bottom(px(8.0)),
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
                        border: if id == active {
                            UiRect::all(px(1))
                        } else {
                            UiRect::ZERO
                        },
                        overflow: Overflow::clip(),
                        border_radius: BorderRadius::bottom(px(8.0)),
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
                        let mut tabs = core.0.lock().unwrap();
                        tabs.user_activity();
                        if tabs.focus(id) {
                            view.terminal = entity;
                            focus.set(entity, FocusCause::Pressed);
                        }
                    },
                )
                .id();
            mouse_input::observe(commands, container, entity, id);
            views.push(PaneView {
                id,
                container,
                entity,
                image,
                cols: 80,
                rows: 24,
                rendered: false,
                active: false,
                paint: raster::PaintState::default(),
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
            // Weighted slots share the entire allocation, with no separator.
            for (tree, weight) in [(first, *ratio), (second, 1.0 - ratio)] {
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
                let child = spawn_pane_tree(commands, images, tree, views, active);
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
    mut borders: Query<(&bevy::feathers::theme::ThemeBorderColor, &mut Node)>,
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
            tabs.active_tab().active_pane,
        );
        commands.entity(view.centre).add_child(root);
        view.pane_root = Some(root);
        view.tree_state = Some(state);
    }
    let active = tabs.active_tab().active_pane;
    for pane in &view.pane_views {
        let width = if pane.id == active {
            UiRect::all(px(1))
        } else {
            UiRect::ZERO
        };
        if let Ok((_, mut node)) = borders.get_mut(pane.container)
            && node.border != width
        {
            node.border = width;
        }
        let token = if pane.id == active {
            ctk::theme::tokens::CONTROL_ACTIVE
        } else {
            ctk::theme::tokens::BORDER
        };
        // The Bus can change focus after Update but before refresh: compare
        // the installed border token, independently of the last drawn cursor.
        if !borders
            .get(pane.container)
            .is_ok_and(|(border, _)| border.0 == token)
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

/// `TERM_RASTER_TRACE=1`: one stderr line per damaged frame with the rows
/// painted, whether the repaint was full, and what it cost.
///
/// It exists because the damage-rect work was easy to *assume* effective and
/// the process-level CPU says nothing about it either way — under a scrolling
/// stream every row is dirty and the rect saves nothing, while a
/// carriage-returned line is one row in twenty-three. This is how that
/// distinction is observed rather than argued about. Read once: a per-frame
/// `env::var_os` walks the environment.
fn raster_trace() -> &'static bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    TRACE.get_or_init(|| {
        std::env::var_os("TERM_RASTER_TRACE").is_some_and(|value| value != "0")
    })
}

/// Layout resolves and rounds each border in physical pixels independently.
fn pane_interior(computed: &ComputedNode) -> Vec2 {
    let border = computed.border();
    let insets = border.min_inset + border.max_inset;
    (computed.size() - insets).max(Vec2::ZERO) * computed.inverse_scale_factor()
}

// Bevy injects these independent resources/queries as system parameters.
#[allow(clippy::too_many_arguments)]
fn refresh(
    core: Res<Core>,
    notify: Res<NotifyTx>,
    painter: Res<Painter>,
    settings: Res<TermSettings>,
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
        let settings = settings.0;
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
        // `grid_snapshot` is `screen(true)` plus the dirty-row flags, so the
        // raster repaints the rows that changed instead of every glyph on the
        // grid; `switched` (first frame, focus change, rebuilt Raster) still
        // forces the lot.
        let snapshot = terminal.grid_snapshot();
        let mut screen = snapshot.screen;
        let dirty = snapshot.dirty_rows;
        // Masking the cursor by focus is a change to what gets painted, and
        // the row it vacates is our own inversion rather than grid damage —
        // `paint` re-marks the row its own recorded cursor was on, so both
        // halves of a focus change are covered without bookkeeping here.
        screen.cursor_visible &= active;
        pane.active = active;
        // One source of truth for the target's extent. The part a hand-rolled
        // `rows * cell_height` gets wrong is the clamp to the whole cell rows
        // the screen actually has cells for, and it fails quietly: `paint`
        // refuses a buffer it cannot fill, writes nothing, and the frame is
        // blank.
        let (width, height) = painter.target_size(&screen);
        let stride = width as usize * 4;
        let bytes = stride * height as usize;
        // A zero extent is not paintable and not installable: `paint` would
        // refuse the buffer, and a zero-sized `Extent3d` is rejected by wgpu's
        // own validation, which is a panic in the render world rather than a
        // blank pane. `grid_snapshot` does not produce one (cols and rows are
        // clamped above), so this is a guard on an invariant held elsewhere —
        // which is exactly the kind that earns its keep when the elsewhere
        // moves.
        if width == 0 || height == 0 {
            pane.rendered = false;
            continue;
        }
        if let Some(mut image) = images.get_mut(&pane.image) {
            // Mutate the texture the pane already owns. Building a fresh
            // `Image` per damaged frame threw away a full-frame buffer (~12 MB
            // at 2.5x scale) every time, and a stream damages at up to 60 fps.
            if image.texture_descriptor.size.width != width
                || image.texture_descriptor.size.height != height
            {
                image.texture_descriptor.size = Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                };
            }
            let rgba = image.data.get_or_insert_with(Vec::new);
            // `paint` never resizes a buffer it does not own, so sizing it is
            // this caller's job.
            //
            // The invalidation is not redundant with the identity check inside
            // `paint`. That check compares the buffer's address and length to
            // what it last painted — and repairing the buffer HERE can restore
            // both: anything that empties the `Vec` while keeping its capacity
            // leaves this `resize` handing back the same address and the same
            // length, with zeroed contents that `paint` would then trust and
            // only partly overwrite. Nothing in bterm does that today; the
            // churn branch was nonetheless right to treat "I had to repair the
            // buffer" as knowledge only the caller has.
            if rgba.len() != bytes {
                rgba.clear();
                rgba.resize(bytes, 0);
                pane.paint.invalidate();
            }
            // `switched` is a first frame, a focus change, or a Raster rebuilt
            // at a new scale. The last of those the state catches by itself
            // (the cell size is part of what it compares); the others are the
            // caller's knowledge, and this is the call that hands it over.
            if switched {
                pane.paint.invalidate();
            }
            let started = Instant::now();
            // The bands borrow the state, so the sum ends the borrow on this
            // statement — everything below wants `pane` back.
            let painted_px: u32 = painter
                .paint(&screen, rgba, stride, &mut pane.paint, &dirty)
                .iter()
                .map(|band| band.height)
                .sum();
            let converted = Instant::now();
            // A refusal is not reported as an error: `paint` invalidates its
            // state and writes nothing. We sized the buffer from `target_size`
            // immediately above, so a refusal here means those two disagree.
            // Either way the texture holds pixels nothing wrote, and leaving
            // `rendered` false is what makes the next frame repaint it whole
            // rather than trust it.
            // Bands are whole cell rows by construction, so both divisions are
            // exact. `full` is OBSERVED here — the count of rows actually
            // painted — where the churn branch printed the decision it had made
            // in advance; a zero-row target would make `rows == total` claim a
            // full repaint of nothing, so it says so instead.
            let rows = painted_px / painter.height.max(1);
            let total = height / painter.height.max(1);
            let refused = pane.paint.grid() == (0, 0) && total > 0;
            if *raster_trace() {
                let full = if total == 0 {
                    "empty"
                } else if rows == total {
                    "true"
                } else {
                    "false"
                };
                eprintln!(
                    "RASTER rows={rows}/{total} full={full} refused={refused} paint={:?}",
                    converted - started
                );
            }
            pane.rendered = !refused;
            let mut stats = terminal.stats.lock().unwrap();
            stats.vt_rgba.add(converted - screen.updated);
            // The raster writes straight into the asset's own buffer, so this
            // is the rasterisation cost on the main thread and nothing else.
            //
            // It is NOT the upload cost, and `uploads` below is not bounded by
            // the damage bands: mutating the asset trips Bevy's change
            // detection and the render world re-uploads the WHOLE image
            // (bevy_render texture/gpu_image.rs). Damage bounds what the CPU
            // paints, not what the GPU is handed — cutting the second one is
            // its own piece of work, and this counter is what would show it.
            stats.raster_paint.add(converted - started);
            stats.uploads += 1;
        } else {
            // No asset to paint into, and `grid_snapshot` has already consumed
            // the damage that said which rows changed. Those rows are gone, so
            // the pane owes a full repaint whenever its texture comes back.
            pane.rendered = false;
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
