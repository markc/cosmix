//! The iced application, in the shape of ced's `app.rs`: state = the
//! [`DopusCore`] plus chrome; `view` composes header · sort headers ·
//! [`rows::FileList`](crate::view::rows::FileList) · status bar. A normal xdg
//! toplevel, `application_id = "dev.cosmix.dopus"`, SingleThread executor,
//! tiny-skia.
//!
//! Event flow (the app contract, `cosmix-dopus-core`'s seven laws):
//! - worker replies arrive on the core's `mpsc::Receiver`; a pumper thread
//!   forwards each into the futures channel the subscription drains, and the
//!   UI thread feeds every one through `core.on_event` exactly once — law 2.
//! - `Msg::Frame` fires per redraw and calls `core.tick(now)` — law 1 (the
//!   same frames make the rows widget re-format relative modified times on
//!   the app's clock).
//! - derived `ConfirmRequested`/`PromptRequested` are answered immediately
//!   (`No`/dismissal): P1 has no dialog surface, and law 3 forbids letting
//!   one wedge — a P2 dialog UI replaces these arms.
//! - derived `OpenFile` becomes a status line: no spawn surface until P3 —
//!   law 4's P1 posture.
//! - sort-column switches pass `ascending: true` — law 5 (the core toggles a
//!   same-column sort itself).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use iced::futures::channel::mpsc::UnboundedReceiver;
use iced::{Element, Size, Subscription, Task};
use iced_tiny_skia::Renderer;

use cosmix_actions::{ActionId, Keymap};
use cosmix_design::{Mode, Scheme};
use cosmix_dopus_core::{ConfigFile, ConfirmAnswer, CoreEvent, DOpusConfig, DopusCore, PaneId, VisibleRow};

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
    /// Resolved chords and menu entries (`view.sort-*` headers, nav icons).
    Actions(Vec<ActionId>),
    /// The listing widget's clicks.
    Rows(rows::RowsMsg),
    /// A raw core event, back from the pumper (law 2's feed).
    Core(CoreEvent),
    /// Window edges (focus reloads the keymap; close quits).
    Window(iced::window::Event),
    /// One redraw (law 1's tick).
    Frame(Instant),
    Noop,
}

pub struct Dopus {
    core: DopusCore,
    /// The listing snapshot `view` draws; refreshed after every update.
    rows: Vec<VisibleRow>,
    router: keys::SharedRouter,
    icons: Icons,
    theme: Theme,
    /// The in-session `theme.*` selection (P1 does not persist it).
    theme_override: Option<(Scheme, Mode)>,
    /// A transient message the next core status replaces.
    status: Option<String>,
    bus: Option<BusHandle>,
    action_table: Vec<ActionRow>,
    dirs: Option<AppDirs>,
    service: String,
    noded_url: String,
    window: Size,
    tint: String,
    quitting: bool,
}

/// Run the windowed app registered on the Bus as `service`, ignoring `paths`
/// (P1 has no open-target handling; the single-instance forward still
/// delivers them to the running instance, which also ignores them).
pub fn run(
    config: DOpusConfig,
    config_file: Option<ConfigFile>,
    dirs: Option<AppDirs>,
    service: &str,
    noded_url: &str,
    paths: &[String],
) -> anyhow::Result<()> {
    let _ = paths;
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
    let router = keys::initial(keymap_path.as_deref())?;
    let action_table = {
        let router = router.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        action_table(&router.keymap)
    };

    let theme = theme::resolve(app_theme_override(dirs.as_ref()).as_deref());
    let tint = icons::hex(theme.tokens.text);
    let icons = Icons::new();
    icons.ensure(&tint, ICON_PX, ICON_SCALE);

    let (core, core_events) = DopusCore::new(config, config_file);
    let mut app = Dopus {
        core,
        rows: Vec::new(),
        router,
        icons,
        theme,
        theme_override: None,
        status: None,
        bus,
        action_table,
        dirs,
        service: service.to_owned(),
        noded_url: noded_url.to_owned(),
        window: Size::new(980.0, 640.0),
        tint: tint.clone(),
        quitting: false,
    };
    app.refresh_rows();
    if let Some(note) = app.theme.notes.clone() {
        app.status = Some(format!("Theme: {note}"));
    }

    let streams = deliveries.map(|deliveries| Streams {
        deliveries,
        core_events: pump(core_events),
    });
    if STREAMS.set(Mutex::new(streams)).is_err() {
        anyhow::bail!("app::run called twice in one process");
    }

    let state = std::cell::RefCell::new(Some(app));
    let ui_font = state.borrow().theme.ui_font;
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
}

static STREAMS: OnceLock<Mutex<Option<Streams>>> = OnceLock::new();

/// Bus deliveries and core events, merged. Built once: iced keeps a
/// `Subscription::run` alive for as long as it is returned.
fn streams() -> impl iced::futures::Stream<Item = Msg> {
    use iced::futures::StreamExt;
    let taken = STREAMS.get().and_then(|m| m.lock().ok()?.take());
    match taken {
        Some(s) => iced::futures::stream::select(s.deliveries.map(Msg::Bus), s.core_events.map(Msg::Core)).boxed(),
        None => {
            tracing::error!("dopus: the delivery streams were already taken; the window will not hear the core");
            iced::futures::stream::empty().boxed()
        }
    }
}

/// `dopus.actions.list`'s table: the P1 actions with their effective chords.
fn action_table(keymap: &Keymap) -> Vec<ActionRow> {
    verbs::action_table(keymap)
}

impl Dopus {
    fn title(&self) -> String {
        let pane = self.core.pane(PaneId::Left);
        format!("{} — CosMix DOpus", cosmix_dopus_core::sanitise_display_path(&pane.path))
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        let task = self.dispatch(msg);
        // The view snapshot: refreshed on every message, so no core mutation
        // can be drawn stale.
        self.refresh_rows();
        task
    }

    fn dispatch(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Bus(delivery) => self.on_delivery(delivery),
            Msg::Actions(actions) => self.on_actions(&actions),
            Msg::Rows(msg) => self.on_rows(msg),
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
            Msg::Noop => Task::none(),
        }
    }

    fn refresh_rows(&mut self) {
        self.rows = self.core.visible_rows(PaneId::Left);
    }

    fn on_rows(&mut self, msg: rows::RowsMsg) -> Task<Msg> {
        match msg {
            rows::RowsMsg::Select(path) => self.core.select_path(PaneId::Left, Some(path)),
            rows::RowsMsg::Toggle(path) => self.core.toggle_expand(PaneId::Left, &path),
        }
        Task::none()
    }

    /// Law 3 and law 4's P1 posture, applied to the core's derived events.
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
                | CoreEvent::OperationArrived { .. } => {}
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
            }
            Delivery::Connected => tracing::info!("Bus connected as `{}`", self.service),
            Delivery::Disconnected => tracing::warn!("Bus disconnected; reconnecting in the background"),
        }
        Task::none()
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
            if let Some(scheme) = verbs::scheme_action(*action) {
                self.set_override(scheme, None);
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

    fn set_override(&mut self, scheme: Option<&str>, mode: Option<Mode>) {
        let current = self.theme_override.take().unwrap_or((self.theme.scheme, self.theme.mode));
        let scheme = scheme.map(|name| Scheme::from_name(name)).flatten().unwrap_or(current.0);
        self.theme_override = Some((scheme, mode.unwrap_or(current.1)));
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
                    let mut router = self.router.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    self.action_table = action_table(&router.keymap);
                }
            }
            iced::window::Event::Resized(size) => self.window = size,
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
        if let Some(bus) = &self.bus {
            bus.quit();
        }
        iced::exit()
    }

    fn subscription(&self) -> Subscription<Msg> {
        Subscription::batch([
            Subscription::run(streams),
            iced::window::frames().map(Msg::Frame),
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

    fn look(&self) -> Look<'_> {
        Look {
            tokens: &self.theme.tokens,
            chrome: &self.theme.chrome,
            ui_font: self.theme.ui_font,
            mono_font: self.theme.mono_font,
            px: self.theme.ui_px(),
            mono_px: self.theme.mono.1,
        }
    }

    fn view(&self) -> Element<'_, Msg, iced::Theme, Renderer> {
        let look = self.look();
        let info = self.status.as_deref().unwrap_or(self.core.info());
        // The router wraps everything: it sees every key before its children
        // and publishes resolved actions (never `event::listen`, which drops
        // keys under load — the ced/term rule).
        keys::router(
            view::root(&look, &self.icons, &self.tint, self.core.pane(PaneId::Left), &self.rows, info),
            self.router.clone(),
            Msg::Actions,
        )
        .into()
    }
}
