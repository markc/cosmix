//! The iced application, in the shape of ced's `app.rs`: state = the
//! [`DopusCore`] plus chrome; `view` composes the Places sidebar · the twin
//! [`panes::pane_column`](crate::view::panes) columns (pane header ·
//! [`location`](crate::view::location) bar · sort headers ·
//! [`rows::FileList`](crate::view::rows::FileList)) with the draggable
//! divider between them, and the status bar. A normal xdg toplevel,
//! `application_id = "dev.cosmix.dopus"`, SingleThread executor, tiny-skia.
//!
//! Event flow (the app contract, `cosmix-dopus-core`'s seven laws):
//! - worker replies arrive on the core's `mpsc::Receiver`; a pumper thread
//!   forwards each into the futures channel the subscription drains, and the
//!   UI thread feeds every one through `core.on_event` exactly once — law 2.
//! - law 1's cadence is threefold: `Msg::Frame` ticks per redraw (which also
//!   re-formats the rows' relative modified times), a 200 ms `Msg::Tick`
//!   heartbeat ticks when the window is idle (frames only fire on redraws),
//!   and `quit` ticks once before exit so pending config persists.
//! - derived `ConfirmRequested`/`PromptRequested` are answered immediately
//!   (`No`/dismissal): P2 has no dialog surface, and law 3 forbids letting
//!   one wedge — a P3 dialog UI replaces these arms.
//! - derived `OpenFile` becomes a status line: no spawn surface until P3 —
//!   law 4's P2 posture.
//! - sort-column switches pass `ascending: true` — law 5 (the core toggles a
//!   same-column sort itself).
//! - the divider drives `set_split_ratio` — law 7 (persistence derives from
//!   core state only).
//!
//! Focus: the key router resolves against a real [`FocusContext`] — while a
//! location bar is being edited the router's `focus_editable` is on, so the
//! chord resolver routes keys into the editor (every default binding is
//! `allow_in_editable: false`). P1 resolved `global()` everywhere; P2
//! threads the focus.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use iced::futures::channel::mpsc::UnboundedReceiver;
use iced::{Element, Size, Subscription, Task};
use iced_tiny_skia::Renderer;

use cosmix_actions::{ActionId, Keymap};
use cosmix_design::{Mode, Scheme};
use cosmix_dopus_core::{ConfigFile, ConfirmAnswer, CoreEvent, DOpusConfig, DopusCore, PaneId, SortColumn, VisibleRow};

use crate::bus::{self, BusHandle, Delivery};
use crate::dirs::AppDirs;
use crate::icons::{self, Icons};
use crate::keys;
use crate::theme::{self, Theme};
use crate::verbs::{self, ActionRow, ServerMeta, Served};
use crate::view::{self, rows, Look};

/// The Wayland application id.
pub const APP_ID: &str = "dev.cosmix.dopus";

/// How often the icons re-raster target size (logical px × scale).
const ICON_PX: u32 = 16;
const ICON_SCALE: u32 = 2;

/// Everything the app reacts to.
#[derive(Debug, Clone)]
pub enum Msg {
    /// From the bus thread.
    Bus(Delivery),
    /// Resolved chords and menu entries (nav icons, theme actions).
    Actions(Vec<ActionId>),
    /// A pane's listing widget: the pane id rides the message (the
    /// [`FileList`](rows::FileList) itself is pane-agnostic).
    PaneRows(PaneId, rows::RowsMsg),
    /// A pane-local control: activate `pane`, then act on it (the core's
    /// pane verbs act on the active pane; this is the activate-then-act
    /// contract the pane headers and sort headers publish).
    Pane(PaneId, PaneOp),
    /// Navigate `pane` to a path (Places clicks, the location bar's submit).
    Go(PaneId, PathBuf),
    /// Begin editing `pane`'s location bar (the bar was clicked).
    LocationEdit(PaneId),
    /// The location editor's text changed (the REAL path text).
    LocationInput(String),
    /// Enter in `pane`'s editor: navigate there.
    LocationSubmit(PaneId),
    /// Escape in the editor: cancel.
    LocationCancel,
    /// The divider moved (ratio clamped 0.1–0.9) or double-clicked (0.5).
    Split(f32),
    /// A raw core event, back from the pumper (law 2's feed).
    Core(CoreEvent),
    /// Window edges (focus reloads the keymap; close quits).
    Window(iced::window::Event),
    /// One redraw (law 1's tick).
    Frame(Instant),
    /// The 200 ms heartbeat (law 1's tick while idle + the chord-deadline poll).
    Tick(Instant),
    Noop,
}

/// A pane-local control, applied after activating its pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneOp {
    NavBack,
    NavForward,
    NavParent,
    NavHome,
    Refresh,
    ToggleHidden,
    Sort(SortColumn),
}

pub struct Dopus {
    core: DopusCore,
    /// The per-pane listing snapshot `view` draws; refreshed after every
    /// update so no core mutation can be drawn stale.
    rows: [Vec<VisibleRow>; 2],
    /// The core's live split ratio, cached for the view (the core owns it;
    /// the divider writes through `set_split_ratio` — law 7).
    split_ratio: f32,
    /// The pane whose location bar is being edited, and the REAL path text
    /// (never display-sanitised — the sanitisation law covers display only).
    editing: Option<(PaneId, String)>,
    router: keys::SharedRouter,
    icons: Icons,
    theme: Theme,
    /// The in-session `theme.*` selection (not persisted).
    theme_override: Option<(Scheme, Mode)>,
    /// A transient message the next core status replaces.
    status: Option<String>,
    bus: Option<BusHandle>,
    action_table: Vec<ActionRow>,
    dirs: Option<AppDirs>,
    service: String,
    tint: String,
    quitting: bool,
}

/// Run the windowed app registered on the Bus as `service`. `paths` are the
/// forwarded `dopus.open` PATHs: the first navigates the left pane, the
/// second the right, extras are logged and ignored (verbs::apply_open_paths).
pub fn run(
    config: DOpusConfig,
    config_file: Option<ConfigFile>,
    dirs: Option<AppDirs>,
    service: &str,
    noded_url: &str,
    paths: &[String],
) -> anyhow::Result<()> {
    let (bus, deliveries) = match bus::spawn(service, noded_url) {
        Ok(started) => (Some(started.0), Some(started.1)),
        Err(bus::StartError::NameTaken) => {
            // Lost the registration race (§ single instance): hand the paths
            // over if the winner answers, else say why we cannot run.
            if bus::probe_running(noded_url, service) {
                return bus::forward_open(noded_url, service, paths)
                    .map_err(|e| anyhow::anyhow!("forwarding to the running dopus: {e}"));
            }
            anyhow::bail!("the Bus name `{service}` is taken, but nothing answers dopus.ping on it");
        }
        Err(bus::StartError::Rejected(message)) => anyhow::bail!("noded refused registration as `{service}`: {message}"),
        // A file manager works standalone: no broker, no Bus.
        Err(bus::StartError::Unreachable(message)) => {
            tracing::info!("running without a Bus: {message}");
            (None, None)
        }
    };

    let keymap_path = dirs.as_ref().map(|d| d.keymap_file());
    let router = keys::initial(keymap_path.as_deref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let action_table = {
        let router = router.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        action_table(&router.keymap)
    };

    let theme = theme::resolve(app_theme_override(dirs.as_ref()).as_deref());
    // Read before `theme` moves into the app: iced's default font.
    let ui_font = theme.ui_font;
    let tint = icons::hex(theme.tokens.text);
    let icons = Icons::new();
    icons.ensure(&tint, ICON_PX, ICON_SCALE);

    let (mut core, core_events) = DopusCore::new(config, config_file);
    // Startup `dopus.open` PATHs land in the panes before the first frame.
    verbs::apply_open_paths(&mut core, paths);
    let split_ratio = core.config_snapshot().split_ratio;
    let mut app = Dopus {
        core,
        rows: [Vec::new(), Vec::new()],
        split_ratio,
        editing: None,
        router,
        icons,
        theme,
        theme_override: None,
        status: None,
        bus,
        action_table,
        dirs,
        service: service.to_owned(),
        tint: tint.clone(),
        quitting: false,
    };
    app.refresh_panes();
    if let Some(note) = app.theme.notes.clone() {
        app.status = Some(format!("Theme: {note}"));
    }

    // Built unconditionally: the core channel is pumped whether or not the
    // Bus exists (a no-broker windowed run still hears the core — law 2).
    // With no Bus, deliveries drain an always-empty channel.
    let streams = Streams {
        deliveries: deliveries
            .unwrap_or_else(|| iced::futures::channel::mpsc::unbounded().1),
        core_events: pump(core_events),
        heartbeat: heartbeat(),
    };
    if STREAMS.set(Mutex::new(Some(streams))).is_err() {
        anyhow::bail!("app::run called twice in one process");
    }

    let state = std::cell::RefCell::new(Some(app));
    iced::application(move || state.borrow_mut().take().expect("iced boots once"), Dopus::update, Dopus::view)
        .executor::<SingleThread>()
        .title(Dopus::title)
        .subscription(Dopus::subscription)
        .theme(|app: &Dopus| app.theme.iced_theme())
        .style(|app: &Dopus, _| iced::theme::Style {
            background_color: app.theme.tokens.surface,
            text_color: app.theme.tokens.text,
        })
        .default_font(ui_font)
        .window(iced::window::Settings {
            size: Size::new(980.0, 640.0),
            min_size: Some(Size::new(420.0, 240.0)),
            exit_on_close_request: false,
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: APP_ID.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("window: {e}"))
}

/// The per-app theme override path, when the directory exists to hold one.
fn app_theme_override(dirs: Option<&AppDirs>) -> Option<PathBuf> {
    dirs.map(AppDirs::theme_override).filter(|p| p.exists())
}

/// Forward raw core events into the UI thread's channel (law 2's transport;
/// the UI thread does the feeding). The receiver is drained forever: a dead
/// UI (channel closed) ends the pump.
fn pump(receiver: std::sync::mpsc::Receiver<CoreEvent>) -> UnboundedReceiver<CoreEvent> {
    let (tx, rx) = iced::futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("dopus-core-events".to_owned())
        .spawn(move || {
            for event in receiver {
                if tx.unbounded_send(event).is_err() {
                    break;
                }
            }
        })
        .expect("spawning the core-event pump");
    rx
}

/// Law 1's idle heartbeat: frames only fire on redraws, so a std thread
/// sends `Instant::now()` every 200 ms into the merged stream — the cadence
/// the config debounce, count dispatch and chord deadlines advance on while
/// the window is idle or occluded (ced's shape: everything through the one
/// `Subscription::run` stream; `iced::time::every` is not in this
/// feature set).
fn heartbeat() -> UnboundedReceiver<Instant> {
    let (tx, rx) = iced::futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("dopus-heartbeat".to_owned())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if tx.unbounded_send(Instant::now()).is_err() {
                return;
            }
        })
        .expect("spawning the heartbeat thread");
    rx
}

/// One background thread for iced's tasks (the `apps/term` executor).
struct SingleThread(iced::futures::executor::ThreadPool);

impl iced::Executor for SingleThread {
    fn new() -> Result<Self, iced::futures::io::Error> {
        iced::futures::executor::ThreadPool::builder()
            .pool_size(1)
            .name_prefix("dopus-task")
            .create()
            .map(Self)
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn_ok(future);
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        iced::futures::executor::block_on(future)
    }
}

/// The receivers the subscription drains, handed over once.
struct Streams {
    deliveries: UnboundedReceiver<Delivery>,
    core_events: UnboundedReceiver<CoreEvent>,
    heartbeat: UnboundedReceiver<Instant>,
}

static STREAMS: OnceLock<Mutex<Option<Streams>>> = OnceLock::new();

/// Bus deliveries, core events and the heartbeat, merged. Built once: iced
/// keeps a `Subscription::run` alive for as long as it is returned.
fn streams() -> impl iced::futures::Stream<Item = Msg> {
    use iced::futures::StreamExt;
    let taken = STREAMS.get().and_then(|m| m.lock().ok()?.take());
    match taken {
        Some(s) => iced::futures::stream::select(
            iced::futures::stream::select(s.deliveries.map(Msg::Bus), s.core_events.map(Msg::Core)),
            s.heartbeat.map(Msg::Tick),
        )
        .boxed(),
        None => {
            tracing::error!("dopus: the delivery streams were already taken; the window will not hear the core");
            iced::futures::stream::empty().boxed()
        }
    }
}

/// `dopus.actions.list`'s table: the served actions with their effective chords.
fn action_table(keymap: &Keymap) -> Vec<ActionRow> {
    verbs::action_table(keymap)
}

impl Dopus {
    fn title(&self) -> String {
        let pane = self.core.pane(self.core.active());
        format!("{} — CosMix DOpus", cosmix_dopus_core::sanitise_display_path(&pane.path))
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        let task = self.dispatch(msg);
        // The view snapshot: refreshed on every message, so no core mutation
        // can be drawn stale.
        self.refresh_panes();
        task
    }

    fn dispatch(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Bus(delivery) => self.on_delivery(delivery),
            Msg::Actions(actions) => self.on_actions(&actions),
            Msg::PaneRows(pane, msg) => self.on_rows(pane, msg),
            Msg::Pane(pane, op) => self.on_pane_op(pane, op),
            Msg::Go(pane, path) => {
                self.core.navigate(pane, path);
                Task::none()
            }
            Msg::LocationEdit(pane) => self.begin_edit(pane),
            Msg::LocationInput(text) => {
                if let Some((_, current)) = &mut self.editing {
                    *current = text;
                }
                Task::none()
            }
            Msg::LocationSubmit(pane) => {
                let text = self.editing.as_ref().map(|(_, text)| text.clone()).unwrap_or_default();
                self.stop_editing();
                // Leading `~` expands to home; the core re-lists and
                // status-lines a path it cannot read.
                self.core.navigate(pane, crate::dirs::expand_tilde(&text));
                Task::none()
            }
            Msg::LocationCancel => {
                self.stop_editing();
                Task::none()
            }
            Msg::Split(ratio) => {
                // Law 7: persistence derives from core state only — the core
                // settles the changed ratio through its own debounce.
                self.core.set_split_ratio(ratio.clamp(view::panes::SPLIT_MIN, view::panes::SPLIT_MAX));
                Task::none()
            }
            Msg::Core(event) => {
                // Law 2: every raw event through on_event exactly once.
                let derived = self.core.on_event(event);
                self.on_derived(derived);
                Task::none()
            }
            Msg::Window(event) => self.on_window(event),
            Msg::Frame(now) => {
                // Law 1: tick every frame. Also ages the status line's
                // replacement cycle: nothing to do, the view re-renders.
                let derived = self.core.tick(now);
                self.on_derived(derived);
                Task::none()
            }
            Msg::Tick(now) => {
                // Law 1 while idle (frames only fire on redraws), plus the
                // chord-deadline poll: an expired chord resolves without
                // waiting for the next keypress.
                let derived = self.core.tick(now);
                self.on_derived(derived);
                let actions = keys::poll_timeout(&self.router);
                if actions.is_empty() {
                    Task::none()
                } else {
                    self.on_actions(&actions)
                }
            }
            Msg::Noop => Task::none(),
        }
    }

    /// Refresh the per-pane view snapshots (listings + the split ratio).
    fn refresh_panes(&mut self) {
        self.rows = [self.core.visible_rows(PaneId::Left), self.core.visible_rows(PaneId::Right)];
        self.split_ratio = self.core.config_snapshot().split_ratio;
    }

    fn on_rows(&mut self, pane: PaneId, msg: rows::RowsMsg) -> Task<Msg> {
        match msg {
            // Any press lands in the listing: clicking a pane makes it the
            // active one (the divider/keyboard follow the same core state).
            rows::RowsMsg::Press => self.core.set_active_pane(pane),
            rows::RowsMsg::Select(path) => self.core.select_path(pane, Some(path)),
            rows::RowsMsg::Toggle(path) => self.core.toggle_expand(pane, &path),
        }
        Task::none()
    }

    /// Activate `pane`, then act on it (the core's pane verbs act on the
    /// active pane). Law 5: a sort-column switch passes `ascending: true`.
    fn on_pane_op(&mut self, pane: PaneId, op: PaneOp) -> Task<Msg> {
        self.core.set_active_pane(pane);
        match op {
            PaneOp::NavBack => self.core.go_back(),
            PaneOp::NavForward => self.core.go_forward(),
            PaneOp::NavParent => self.core.go_parent(),
            PaneOp::NavHome => self.core.go_home(),
            PaneOp::Refresh => self.core.refresh(),
            PaneOp::ToggleHidden => self.core.toggle_hidden(),
            PaneOp::Sort(column) => self.core.set_sort(column, true),
        }
        Task::none()
    }

    /// Enter edit mode on `pane`'s location bar: activate the pane (the bar
    /// was clicked, so that pane takes the keyboard), seed the editor with
    /// the pane's REAL path (the sanitisation law covers display text only)
    /// and hand keyboard focus to the field.
    fn begin_edit(&mut self, pane: PaneId) -> Task<Msg> {
        self.core.set_active_pane(pane);
        let path = pane_path_text(&self.core, pane);
        self.editing = Some((pane, path));
        keys::set_focus_editable(&self.router, true);
        Task::batch([
            iced::widget::operation::focus(view::location::location_id(pane)),
            iced::widget::operation::select_all(view::location::location_id(pane)),
        ])
    }

    /// Leave edit mode (submit, cancel) and give the chords back.
    fn stop_editing(&mut self) {
        if self.editing.take().is_some() {
            keys::set_focus_editable(&self.router, false);
        }
    }

    /// Law 3 and law 4's P2 posture, applied to the core's derived events.
    fn on_derived(&mut self, events: Vec<CoreEvent>) {
        for event in events {
            match event {
                CoreEvent::ConfirmRequested { token, .. } => self.core.confirm(token, ConfirmAnswer::No),
                CoreEvent::PromptRequested { token, .. } => self.core.prompt_text(token, None),
                CoreEvent::OpenFile(path) => {
                    self.status = Some(format!(
                        "Opening {} needs the P3 file operations",
                        cosmix_dopus_core::sanitise_display_path(&path)
                    ));
                }
                CoreEvent::Status { .. } | CoreEvent::InfoChanged | CoreEvent::SelectionChanged { .. } => {}
                CoreEvent::ListingStarted { .. }
                | CoreEvent::ListingArrived { .. }
                | CoreEvent::CountArrived { .. }
                | CoreEvent::OperationArrived { .. }
                // The view re-renders from the core after every message, so
                // "config persisted" and "both panes stale" need no reaction.
                | CoreEvent::ConfigSettled(_)
                | CoreEvent::RefreshAll => {}
            }
        }
    }

    fn on_delivery(&mut self, delivery: Delivery) -> Task<Msg> {
        match delivery {
            Delivery::Command(command) => self.serve(&command),
            Delivery::ThemeChanged => {
                // The shared theme selection changed under us: drop the
                // in-session override and re-resolve from the files.
                self.theme_override = None;
                self.reload_theme();
                Task::none()
            }
            Delivery::Connected => {
                tracing::info!("Bus connected as `{}`", self.service);
                Task::none()
            }
            Delivery::Disconnected => {
                tracing::warn!("Bus disconnected; reconnecting in the background");
                Task::none()
            }
        }
    }

    /// Answer one Bus command through the shared serving layer.
    fn serve(&mut self, command: &bus::Command) -> Task<Msg> {
        let Some(bus) = &self.bus else { return Task::none() };
        let handle = bus.clone();
        let meta = ServerMeta {
            service: self.service.clone(),
            headless: false,
            config_path: self.dirs.as_ref().map(|d| d.config_dir().join("config.conf.mix").display().to_string()),
            theme_scheme: self.theme.scheme.name().to_owned(),
            theme_mode: self.theme.mode.name().to_owned(),
            actions: self.action_table.clone(),
        };
        let info = cosmix_buildinfo::build_info!();
        for served in verbs::serve_command(command, &mut self.core, &meta, &info) {
            match served {
                Served::Reply { id, rc, body } => handle.respond(id, rc, body),
                Served::ThemeSet { id, scheme, mode } => {
                    match self.select_theme(scheme.as_deref(), mode.as_deref()) {
                        Ok(()) => handle.respond(
                            id,
                            0,
                            serde_json::to_string(&verbs::ThemeSetReply {
                                scheme: self.theme.scheme.name().to_owned(),
                                mode: self.theme.mode.name().to_owned(),
                            })
                            .unwrap_or_default(),
                        ),
                        Err(message) => handle.respond(
                            id,
                            10,
                            serde_json::to_string(&verbs::Refusal {
                                error_code: verbs::code::INVALID_ARGUMENT.to_owned(),
                                message,
                                reason: None,
                            })
                            .unwrap_or_default(),
                        ),
                    }
                }
                Served::Quit { id } => {
                    handle.respond(
                        id,
                        0,
                        serde_json::to_string(&verbs::QuitReply { quitting: true }).unwrap_or_default(),
                    );
                    return self.quit();
                }
            }
        }
        Task::none()
    }

    /// The keyboard/menu path: `theme.*` here, everything else through the
    /// shared [`verbs::apply_action`].
    fn on_actions(&mut self, actions: &[ActionId]) -> Task<Msg> {
        let mut quit = false;
        for action in actions {
            if *action == cosmix_actions::theme::MODE_TOGGLE {
                let mode = match self.theme_override.map(|(_, m)| m).unwrap_or(self.theme.mode) {
                    Mode::Dark => Mode::Light,
                    _ => Mode::Dark,
                };
                self.set_override(None, Some(mode));
                continue;
            }
            if let Some(name) = verbs::scheme_action(*action) {
                // The action names are exactly the scheme names, so this
                // always parses; a stray name falls back to the current
                // scheme inside set_override.
                self.set_override(Scheme::from_name(name), None);
                continue;
            }
            match verbs::apply_action(*action, &mut self.core) {
                Ok(verbs::Applied::Done) => {}
                Ok(verbs::Applied::Quit) => quit = true,
                Err(refusal) => self.status = Some(refusal.message),
            }
        }
        if quit {
            return self.quit();
        }
        Task::none()
    }

    /// An in-session theme selection, expressed as names (the Bus path).
    fn select_theme(&mut self, scheme: Option<&str>, mode: Option<&str>) -> Result<(), String> {
        let scheme = scheme
            .map(|name| Scheme::from_name(name).ok_or_else(|| format!("unknown scheme {name:?}")))
            .transpose()?;
        let mode = mode
            .map(|name| Mode::from_name(name).ok_or_else(|| format!("unknown mode {name:?}")))
            .transpose()?;
        self.set_override(scheme, mode);
        Ok(())
    }

    fn set_override(&mut self, scheme: Option<Scheme>, mode: Option<Mode>) {
        let current = self.theme_override.take().unwrap_or((self.theme.scheme, self.theme.mode));
        self.theme_override = Some((scheme.unwrap_or(current.0), mode.unwrap_or(current.1)));
        self.reload_theme();
    }

    /// Re-resolve the theme from the files plus the in-session override, and
    /// re-tint the icons to the new text token.
    fn reload_theme(&mut self) {
        self.theme = theme::resolve_selected(self.theme_override, app_theme_override(self.dirs.as_ref()).as_deref());
        if let Some(note) = self.theme.notes.clone() {
            self.status = Some(format!("Theme: {note}"));
        }
        self.tint = icons::hex(self.theme.tokens.text);
        self.icons.ensure(&self.tint, ICON_PX, ICON_SCALE);
    }

    fn on_window(&mut self, event: iced::window::Event) -> Task<Msg> {
        match event {
            iced::window::Event::Focused => {
                // filemgr's `reload_keymap_on_focus` rule: pick up keymap
                // edits, cancel a pending chord either way.
                let keymap_path = self.dirs.as_ref().map(|d| d.keymap_file());
                keys::reload(&self.router, keymap_path.as_deref());
                {
                    let router = self.router.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    self.action_table = action_table(&router.keymap);
                }
            }
            iced::window::Event::CloseRequested => return self.quit(),
            _ => {}
        }
        Task::none()
    }

    fn quit(&mut self) -> Task<Msg> {
        if self.quitting {
            return Task::none();
        }
        self.quitting = true;
        // Law 1's final tick: persist the pending config before the window
        // (and its frames) go away.
        let derived = self.core.tick(Instant::now());
        self.on_derived(derived);
        if let Some(bus) = &self.bus {
            bus.quit();
            // Reply-then-exit, the windowed twin of headless's join: the
            // drain-before-break flushes any queued reply, and this wait
            // puts it on the wire before iced exits the process.
            bus.wait_done(std::time::Duration::from_secs(3));
        }
        iced::exit()
    }

    fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            Subscription::run(streams),
            iced::window::frames().map(Msg::Frame),
            // The idle heartbeat rides the merged stream (see `heartbeat`).
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Window(
                    e @ (iced::window::Event::Resized(_)
                    | iced::window::Event::Focused
                    | iced::window::Event::CloseRequested),
                ) => Some(Msg::Window(e)),
                _ => None,
            }),
        ])
    }

    fn look(&self) -> Look {
        Look {
            tokens: self.theme.tokens,
            chrome: self.theme.chrome,
            ui_font: self.theme.ui_font,
            mono_font: self.theme.mono_font,
            px: self.theme.ui_px(),
            mono_px: self.theme.mono.1,
        }
    }

    fn view(&self) -> Element<'_, Msg, iced::Theme, Renderer> {
        let info = self.status.as_deref().unwrap_or(self.core.info());
        let editing = self.editing.as_ref().map(|(pane, text)| (*pane, text.as_str()));
        // The router wraps everything: it sees every key before its children
        // and publishes resolved actions (never `event::listen`, which drops
        // keys under load — the ced/term rule).
        keys::router(
            view::root(
                self.look(),
                &self.icons,
                &self.tint,
                self.core.active(),
                self.split_ratio,
                self.core.pane(PaneId::Left),
                self.core.pane(PaneId::Right),
                &self.rows[0],
                &self.rows[1],
                editing,
                info,
            ),
            self.router.clone(),
            Msg::Actions,
        )
        .into()
    }
}

/// The pane's REAL path as editor text (`to_string_lossy`: paths are OsStr;
/// non-UTF-8 bytes cannot be edited in a text field and round-trip as the
/// lossy form).
fn pane_path_text(core: &DopusCore, pane: PaneId) -> String {
    core.pane(pane).path.to_string_lossy().into_owned()
}
