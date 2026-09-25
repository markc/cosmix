//! The iced application (ced E1 plan §2, §4.4): state = the Controller plus
//! chrome state; `view` composes menu bar · tab strip · infobar ·
//! [`EditorWidget`](crate::editor::widget::EditorWidget) · find bar ·
//! problems/output panel · status bar · modal dialogs. A normal xdg toplevel,
//! `application_id = "dev.cosmix.ced"`, SingleThread executor, tiny-skia.
//!
//! The Controller owns every buffer and every `edit.*` request; this module
//! only turns window input into controller calls (UI intents, `human:ced`),
//! performs the controller's [`Effect`]s (Bus sends go to the bus thread,
//! clipboard and exit go to iced), and draws. Deliveries from the bus thread
//! and the chrome's one-shot timers arrive through one `Subscription` fed by
//! futures channels — nothing here polls.
//!
//! Quitting detaches (D13): buffers stay in the edit service, the session
//! remembers the tabs, and there is no save prompt on exit.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::model::{EditCommand, Motion};
use cosmix_edit_client::types::{Intent, Level, Notice, TabId};
use iced::futures::channel::mpsc::UnboundedReceiver;
use iced::keyboard::{Key, key::Named};
use iced::widget::{column, container, stack};
use iced::{Element, Length, Size, Subscription, Task};

use crate::actions::ActionId;
use crate::bus::{self, BusHandle, Delivery};
use crate::chrome::dialogs::confirm::{Choice, Confirm};
use crate::chrome::dialogs::file::{FileDialog, FileMode, FileOutcome};
use crate::chrome::dialogs::goto::Goto;
use crate::chrome::dialogs::recovered::{Recovered, RecoveredMsg};
use crate::chrome::dialogs::{DialogCtx, DialogMsg, Modal};
use crate::chrome::find::{FIND_INPUT, FindBar, FindMsg};
use crate::chrome::infobar::{Info, InfoAction, InfoKey};
use crate::chrome::menu::{BAR_ID, MenuCtx};
use crate::chrome::output::Output;
use crate::chrome::timer::{TimerKey, Timers};
use crate::chrome::{self, Look, MENU_H, STATUS_H, TABS_H};
use crate::config::{self, Config, FONT_PX_MAX, FONT_PX_MIN};
use crate::controller::{Controller, Effect, MatchQuery, Prompt, Tab};
use crate::dirs::{AppDirs, COMPONENT};
use crate::editor::widget::EditorWidget;
use crate::editor::{EditorMsg, EditorView, LayoutReport};
use crate::keys::{self, Binding, Bindings, Routed};
use crate::macros::{self, MacroDef, MacroEnv, MacroEvent};
use crate::theme::{self, Theme};
use crate::verbs::{LayoutReply, Rect};

pub const APP_ID: &str = "dev.cosmix.ced";

/// Change markers clear this long after the tab is focused (§4.5).
const MARKER_CLEAR_MS: u64 = 2000;
/// A transient status message stays this long.
const STATUS_MS: u64 = 6000;

/// Everything the app reacts to.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// From the bus thread.
    Bus(Delivery),
    /// A chrome one-shot timer fired.
    Timer(TimerKey),
    /// A menu entry or a chord (window input: `human:ced`).
    Action(ActionId),
    RunMacro(String),
    Macro(MacroEvent),
    OpenMenu(usize),
    Editor(TabId, EditorMsg),
    SelectTab(TabId),
    CloseTab(TabId),
    Find(FindMsg),
    FindFocused,
    Dialog(DialogMsg),
    Info(InfoAction),
    Paste(Intent, Option<String>),
    Lint(TabId, cosmix_edit_client::highlight::ResultTag, Result<String, String>),
    /// A Mix relex finished off the UI thread.
    Relex(TabId, cosmix_edit_client::highlight::ResultTag, Vec<(std::ops::Range<usize>, cosmix_edit_client::highlight::TokenClass)>),
    GotoOffset(usize),
    JumpLastRemote,
    ClosePanel,
    Escape,
    FileTab,
    DialogKey(Named),
    Zoom(f32),
    Window(iced::window::Event),
    Frame(Instant),
    Noop,
}

/// Which bottom panel is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Problems,
    Output,
}

/// Per-tab lint bookkeeping: what the last lint saw, so a new one runs only
/// on open, after a save, and 1 s after the last edit.
#[derive(Debug, Clone, Default)]
struct LintTrack {
    seen_gen: Option<u64>,
    seen_saved_rev: Option<Option<u64>>,
    inflight: bool,
    note: Option<String>,
}

pub struct App {
    controller: Controller,
    bus: BusHandle,
    timers: Timers,
    dirs: Option<AppDirs>,
    config: Config,
    theme: Theme,
    zoom_px: Option<u16>,
    whitespace: bool,
    line_numbers: bool,
    remote_carets: bool,
    panel: Option<Panel>,
    modal: Option<Modal>,
    find: FindBar,
    /// A chrome text field (the find bar) holds the keyboard.
    field_focused: bool,
    window_focused: bool,
    window: Size,
    bindings: Bindings,
    macros: Vec<MacroDef>,
    macro_running: Option<String>,
    /// A macro asked for while the pipeline was busy (§4.9: wait for idle).
    macro_pending: Option<(String, TabId)>,
    output: Output,
    lint: HashMap<TabId, LintTrack>,
    dismissed: HashSet<InfoKey>,
    /// Warnings / errors the controller posted, newest last.
    notices: Vec<(u64, Option<TabId>, Level, String)>,
    notice_seq: u64,
    status: Option<String>,
    last_active: Option<TabId>,
    /// For `ced.stats`: when the last key was dispatched, not yet framed.
    key_at: Option<Instant>,
    view_us: Cell<u64>,
    quitting: bool,
    empty_diag: Diagnostics,
}

// ── boot ────────────────────────────────────────────────────────────────────

/// The receivers the subscription drains, handed over once.
struct Streams {
    deliveries: UnboundedReceiver<Delivery>,
    timers: UnboundedReceiver<TimerKey>,
}

static STREAMS: OnceLock<Mutex<Option<Streams>>> = OnceLock::new();

/// Run the windowed app registered on the Bus as `service`, opening `paths`.
pub fn run(service: &str, config: Config, paths: Vec<String>) -> anyhow::Result<()> {
    let (bus, deliveries) = match bus::spawn(service) {
        Ok(started) => started,
        Err(bus::StartError::NameTaken) => {
            // Lost the registration race to another ced (§4.8): hand it the
            // paths if it answers, otherwise say why we cannot run.
            if bus::probe_running(service) {
                return bus::forward_open(service, &paths).map_err(|e| anyhow::anyhow!("forwarding to the running ced: {e}"));
            }
            anyhow::bail!("the Bus name `{service}` is taken, but nothing answers ced.ping on it");
        }
        Err(bus::StartError::Rejected(message)) => anyhow::bail!("noded refused registration as `{service}`: {message}"),
        Err(bus::StartError::Unreachable(message)) => anyhow::bail!("no Bus broker: {message}"),
    };
    let (timers, fired) = Timers::start();
    let installed = STREAMS.set(Mutex::new(Some(Streams { deliveries, timers: fired })));
    if installed.is_err() {
        anyhow::bail!("app::run called twice in one process");
    }
    let dirs = AppDirs::resolve(COMPONENT);
    let theme = theme::resolve(dirs.as_ref().map(|d| d.theme_override()).as_deref());
    let ui_font = theme.ui_font;
    let run_id: u32 = rand::random();
    let controller = Controller::new(config.clone(), run_id, false);
    let mut app = App {
        controller,
        bus,
        timers,
        whitespace: config.show_whitespace,
        line_numbers: config.line_numbers,
        remote_carets: config.remote_carets,
        zoom_px: None,
        dirs,
        config,
        theme,
        panel: None,
        modal: None,
        find: FindBar { case: false, ..FindBar::default() },
        field_focused: false,
        window_focused: true,
        window: Size::new(1100.0, 760.0),
        bindings: Bindings::new([]),
        macros: Vec::new(),
        macro_running: None,
        macro_pending: None,
        output: Output::default(),
        lint: HashMap::new(),
        dismissed: HashSet::new(),
        notices: Vec::new(),
        notice_seq: 0,
        status: None,
        last_active: None,
        key_at: None,
        view_us: Cell::new(0),
        quitting: false,
        empty_diag: Diagnostics::default(),
    };
    app.reload_macros();
    if let Some(note) = app.theme.notes.clone() {
        app.post(None, Level::Warn, format!("Theme: {note}"));
    }
    let session_path = app.dirs.as_ref().map(AppDirs::session_file);
    app.controller.set_paths(
        app.dirs.as_ref().map(|d| d.config_file().to_string_lossy().into_owned()),
        session_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
    );
    if let Some(p) = &session_path {
        app.controller.set_session(crate::session::load(p));
    }
    let mut effects = app.controller.start();
    if !paths.is_empty() {
        effects.extend(app.controller.open_paths(&paths, Intent::ui(0)));
    }
    let boot = app.perform(effects);
    let state = std::cell::RefCell::new(Some((app, boot)));
    iced::application(move || state.borrow_mut().take().expect("iced boots once"), App::update, App::view)
        .executor::<SingleThread>()
        .title(App::title)
        .subscription(App::subscription)
        .theme(|app: &App| app.theme.iced_theme())
        .style(|app: &App, _| iced::theme::Style { background_color: app.theme.tokens.surface, text_color: app.theme.tokens.text })
        .default_font(ui_font)
        .window(iced::window::Settings {
            size: Size::new(1100.0, 760.0),
            min_size: Some(Size::new(420.0, 240.0)),
            exit_on_close_request: false,
            platform_specific: iced::window::settings::PlatformSpecific { application_id: APP_ID.to_owned(), ..Default::default() },
            ..Default::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("window: {e}"))
}

/// One background thread for iced's tasks (the `apps/term` executor): ced's
/// tasks are clipboard reads, widget operations and the lint/macro futures,
/// which already run on their own threads.
struct SingleThread(iced::futures::executor::ThreadPool);

impl iced::Executor for SingleThread {
    fn new() -> Result<Self, iced::futures::io::Error> {
        iced::futures::executor::ThreadPool::builder().pool_size(1).name_prefix("ced-task").create().map(Self)
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn_ok(future);
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        iced::futures::executor::block_on(future)
    }
}

/// Bus deliveries and timer firings, merged. Built once: iced keeps a
/// `Subscription::run` alive for as long as it is returned.
fn streams() -> impl iced::futures::Stream<Item = Msg> {
    use iced::futures::StreamExt;
    let taken = STREAMS.get().and_then(|m| m.lock().ok()?.take());
    match taken {
        Some(s) => iced::futures::stream::select(s.deliveries.map(Msg::Bus), s.timers.map(Msg::Timer)).boxed(),
        None => {
            tracing::error!("ced: the delivery streams were already taken; the window will not hear the Bus");
            iced::futures::stream::empty().boxed()
        }
    }
}

// ── update ──────────────────────────────────────────────────────────────────

impl App {
    fn title(&self) -> String {
        match self.active_tab() {
            Some(tab) => {
                let dirty = tab.mirror.as_ref().is_some_and(|m| m.meta().dirty);
                format!("{}{} — ced", if dirty { "● " } else { "" }, chrome::tabs::display_name(tab))
            }
            None => "ced".to_owned(),
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        let id = self.controller.active()?;
        self.controller.tabs().iter().find(|t| t.id == id)
    }

    fn tab(&self, id: TabId) -> Option<&Tab> {
        self.controller.tabs().iter().find(|t| t.id == id)
    }

    fn update(&mut self, msg: Msg) -> Task<Msg> {
        let started = Instant::now();
        let task = self.dispatch(msg);
        let task = Task::batch([task, self.after_transition()]);
        let us = started.elapsed().as_micros() as u64 + self.view_us.get();
        self.controller.record_frame(us, None);
        task
    }

    fn dispatch(&mut self, msg: Msg) -> Task<Msg> {
        match msg {
            Msg::Bus(delivery) => self.on_delivery(delivery),
            Msg::Timer(key) => self.on_timer(key),
            Msg::Action(action) => self.on_ui_action(action),
            Msg::RunMacro(stem) => self.run_macro(stem),
            Msg::Macro(event) => self.on_macro(event),
            Msg::OpenMenu(index) => iced::advanced::widget::operate(cosmix_iced_widgets::menu::open_operation(BAR_ID, index))
                .discard()
                .chain(Task::done(Msg::Noop)),
            Msg::Editor(tab, msg) => self.on_editor(tab, msg),
            Msg::SelectTab(tab) => {
                let effects = self.controller.select_tab(tab);
                self.perform(effects)
            }
            Msg::CloseTab(tab) => {
                let effects = self.controller.on_action(Some(tab), ActionId::FileClose, Intent::ui(tab));
                self.perform(effects)
            }
            Msg::Find(msg) => self.on_find(msg),
            Msg::FindFocused => {
                self.field_focused = true;
                Task::none()
            }
            Msg::Dialog(msg) => self.on_dialog(msg),
            Msg::Info(action) => self.on_info(action),
            Msg::Paste(intent, text) => {
                let effects = self.controller.on_paste(intent, text);
                self.perform(effects)
            }
            Msg::Lint(tab, tag, result) => {
                if let Some(track) = self.lint.get_mut(&tab) {
                    track.inflight = false;
                    track.note = result.as_ref().err().cloned();
                }
                let effects = self.controller.on_lint(tab, tag, result);
                self.perform(effects)
            }
            Msg::Relex(tab, tag, spans) => {
                self.controller.on_relex(tab, tag, spans);
                Task::none()
            }
            Msg::GotoOffset(offset) => self.move_caret(offset),
            Msg::JumpLastRemote => {
                let at = self.active_tab().and_then(|t| t.mirror.as_ref()?.last_remote()?.span.clone()).map(|s| s.start);
                at.map_or_else(Task::none, |offset| self.move_caret(offset))
            }
            Msg::ClosePanel => {
                self.panel = None;
                Task::none()
            }
            Msg::Escape => self.on_escape(),
            Msg::FileTab => {
                if let Some(Modal::File(d)) = &mut self.modal {
                    d.update(crate::chrome::dialogs::file::FileMsg::Complete);
                    return iced::widget::operation::move_cursor_to_end(crate::chrome::dialogs::file::PATH_INPUT);
                }
                Task::none()
            }
            Msg::DialogKey(named) => {
                if let Some(Modal::File(d)) = &mut self.modal {
                    use crate::chrome::dialogs::file::FileMsg;
                    match named {
                        Named::ArrowUp => d.update(FileMsg::Up),
                        Named::ArrowDown => d.update(FileMsg::Down),
                        _ => None,
                    };
                }
                Task::none()
            }
            Msg::Zoom(lines) => {
                let action = if lines > 0.0 { ActionId::ViewZoomIn } else { ActionId::ViewZoomOut };
                self.on_ui_action(action)
            }
            Msg::Window(event) => self.on_window(event),
            Msg::Frame(at) => {
                if let Some(key_at) = self.key_at.take() {
                    let next = at.saturating_duration_since(key_at).as_micros() as u64;
                    self.controller.record_frame(self.view_us.get(), Some(next));
                }
                Task::none()
            }
            Msg::Noop => Task::none(),
        }
    }

    /// Perform the controller's effects.
    fn perform(&mut self, effects: Vec<Effect>) -> Task<Msg> {
        let mut tasks = Vec::new();
        for effect in effects {
            match effect {
                Effect::Send { .. } | Effect::Respond { .. } | Effect::Timer { .. } | Effect::Subscribe { .. } => {
                    self.bus.perform(&effect);
                }
                Effect::Notice { tab, notice } => self.on_notice(tab, notice),
                Effect::ClipboardWrite { text, primary } => tasks.push(if primary {
                    iced::clipboard::write_primary(text)
                } else {
                    iced::clipboard::write(text)
                }),
                Effect::ClipboardRead { primary, intent } => {
                    let read = if primary { iced::clipboard::read_primary() } else { iced::clipboard::read() };
                    tasks.push(read.map(move |text| Msg::Paste(intent.clone(), text)));
                }
                Effect::Prompt(prompt) => self.on_prompt(prompt),
                Effect::WindowAction { tab, action, args, intent } => tasks.push(self.on_window_action(tab, action, args, intent)),
                Effect::Relex { tab, tag, source } => tasks.push(Task::perform(relex(tag, source), move |(tag, spans)| Msg::Relex(tab, tag, spans))),
                // The controller debounces session writes itself.
                Effect::SaveSession => self.save_session(),
                Effect::Quit => {
                    self.bus.perform(&effect);
                    tasks.push(self.quit());
                }
            }
        }
        Task::batch(tasks)
    }

    fn on_delivery(&mut self, delivery: Delivery) -> Task<Msg> {
        match delivery {
            Delivery::Incoming(incoming) => {
                if let cosmix_edit_client::types::Incoming::Topic { topic, .. } = &incoming
                    && topic == "theme.changed"
                {
                    self.reload_theme();
                }
                let effects = self.controller.on_incoming(incoming);
                self.perform(effects)
            }
            Delivery::Command(cmd) => {
                let effects = self.controller.on_bus_command(cmd);
                self.perform(effects)
            }
        }
    }

    fn on_timer(&mut self, key: TimerKey) -> Task<Msg> {
        match key {
            TimerKey::SessionSave => {
                self.save_session();
                Task::none()
            }
            TimerKey::Lint(tab) => self.start_lint(tab),
            TimerKey::FindHighlight => self.push_match_query(),
            TimerKey::ClearMarkers(tab) => {
                if self.controller.active() == Some(tab) && self.window_focused {
                    let effects = self.controller.on_action(Some(tab), ActionId::ViewClearMarkers, Intent::ui(tab));
                    return self.perform(effects);
                }
                Task::none()
            }
            TimerKey::StatusExpiry => {
                self.status = None;
                Task::none()
            }
        }
    }

    fn on_ui_action(&mut self, action: ActionId) -> Task<Msg> {
        let tab = self.controller.active();
        let intent = Intent::ui(tab.unwrap_or(0));
        match action {
            ActionId::FileOpen => return self.open_dialog(FileMode::Open, None),
            ActionId::FileSaveAs => {
                let Some(tab) = tab else { return Task::none() };
                let name = self.tab(tab).map(chrome::tabs::display_name);
                return self.open_dialog(FileMode::SaveAs { tab, intent }, name);
            }
            ActionId::FileSave => {
                // A scratch buffer has no path: Save means Save As.
                if let Some(t) = tab
                    && self.tab(t).is_some_and(|t| t.mirror.as_ref().is_some_and(|m| m.meta().path.is_none()))
                {
                    let name = self.tab(t).map(chrome::tabs::display_name);
                    return self.open_dialog(FileMode::SaveAs { tab: t, intent }, name);
                }
            }
            ActionId::FileExit => return self.quit(),
            ActionId::SearchFind | ActionId::SearchReplace => {
                self.find.open = true;
                self.find.replace = action == ActionId::SearchReplace;
                self.field_focused = true;
                // Seed the pattern from a one-line selection, as Notepad++ does.
                if let Some(seed) = self.selection_text(256).filter(|s| !s.contains('\n') && !s.is_empty()) {
                    self.find.pattern = seed;
                }
                return Task::batch([
                    iced::widget::operation::focus(FIND_INPUT),
                    iced::widget::operation::select_all(FIND_INPUT),
                ]);
            }
            ActionId::SearchFindNext | ActionId::SearchFindPrev => {
                if self.find.pattern.is_empty() {
                    return self.on_ui_action(ActionId::SearchFind);
                }
                let effects = self.controller.on_action_args(tab, action, Some(self.find.args()), intent);
                return self.perform(effects);
            }
            ActionId::SearchReplaceAll => {
                if self.find.pattern.is_empty() {
                    return self.on_ui_action(ActionId::SearchReplace);
                }
                let effects = self.controller.on_action_args(tab, action, Some(self.find.replace_args()), intent);
                return self.perform(effects);
            }
            ActionId::SearchGotoLine => {
                if let Some(m) = self.active_tab().and_then(|t| t.mirror.as_ref()) {
                    self.modal = Some(Modal::Goto(Goto::new(m.text().line_count())));
                    return iced::widget::operation::focus(crate::chrome::dialogs::goto::INPUT);
                }
                return Task::none();
            }
            ActionId::ViewZoomIn | ActionId::ViewZoomOut | ActionId::ViewZoomReset => {
                let base = self.base_px();
                self.zoom_px = match action {
                    ActionId::ViewZoomReset => None,
                    ActionId::ViewZoomIn => Some((self.text_px() as u16 + 1).min(FONT_PX_MAX)),
                    _ => Some((self.text_px() as u16).saturating_sub(1).max(FONT_PX_MIN)),
                };
                if self.zoom_px == Some(base as u16) {
                    self.zoom_px = None;
                }
                return Task::none();
            }
            ActionId::ViewWhitespace => self.whitespace = !self.whitespace,
            ActionId::ViewLineNumbers => self.line_numbers = !self.line_numbers,
            ActionId::ViewRemoteCarets => self.remote_carets = !self.remote_carets,
            ActionId::ViewProblems => self.panel = if self.panel == Some(Panel::Problems) { None } else { Some(Panel::Problems) },
            ActionId::ViewOutput => self.panel = if self.panel == Some(Panel::Output) { None } else { Some(Panel::Output) },
            ActionId::ViewReloadSettings => {
                self.reload_settings();
                return Task::none();
            }
            ActionId::HelpKeys => {
                self.modal = Some(Modal::Keys);
                return Task::none();
            }
            ActionId::HelpAbout => {
                self.modal = Some(Modal::About);
                return Task::none();
            }
            _ => {}
        }
        if matches!(
            action,
            ActionId::ViewWhitespace
                | ActionId::ViewLineNumbers
                | ActionId::ViewRemoteCarets
                | ActionId::ViewProblems
                | ActionId::ViewOutput
        ) {
            return Task::none();
        }
        let effects = self.controller.on_action(tab, action, intent);
        self.perform(effects)
    }

    fn on_editor(&mut self, tab: TabId, msg: EditorMsg) -> Task<Msg> {
        match &msg {
            EditorMsg::Layout(report) => {
                let reply = self.layout_reply(tab, *report);
                self.controller.set_layout(Some(reply));
                return Task::none();
            }
            EditorMsg::Focus(true) => self.field_focused = false,
            EditorMsg::Command(_) | EditorMsg::ImeCommit(_) => {
                self.key_at.get_or_insert_with(Instant::now);
                self.field_focused = false;
            }
            _ => {}
        }
        let effects = self.controller.on_editor(tab, msg);
        self.perform(effects)
    }

    fn move_caret(&mut self, offset: usize) -> Task<Msg> {
        let Some(tab) = self.controller.active() else { return Task::none() };
        let effects = self.controller.on_editor(tab, EditorMsg::Command(EditCommand::Move { to: Motion::To(offset), extend: false }));
        self.perform(effects)
    }

    fn on_find(&mut self, msg: FindMsg) -> Task<Msg> {
        self.find.update(&msg);
        match msg {
            FindMsg::Next => self.on_ui_action(ActionId::SearchFindNext),
            FindMsg::Prev => self.on_ui_action(ActionId::SearchFindPrev),
            FindMsg::Replace => {
                let tab = self.controller.active();
                let effects = self.controller.on_action_args(tab, ActionId::SearchReplace, Some(self.find.replace_args()), Intent::ui(tab.unwrap_or(0)));
                self.perform(effects)
            }
            FindMsg::ReplaceAll => self.on_ui_action(ActionId::SearchReplaceAll),
            FindMsg::Close => {
                self.field_focused = false;
                self.timers.cancel(TimerKey::FindHighlight);
                self.push_match_query()
            }
            FindMsg::Pattern(_) | FindMsg::ToggleCase | FindMsg::ToggleRegex | FindMsg::ToggleWord => {
                self.timers.arm(TimerKey::FindHighlight, 100);
                Task::none()
            }
            FindMsg::Replacement(_) => Task::none(),
        }
    }

    /// Hand the find bar's query to the controller for highlight-all (None
    /// when the bar is closed or empty).
    fn push_match_query(&mut self) -> Task<Msg> {
        let Some(tab) = self.controller.active() else { return Task::none() };
        let query = (self.find.open && !self.find.pattern.is_empty()).then(|| {
            let (pattern, regex) = crate::chrome::find::wire_pattern(&self.find.pattern, self.find.regex, self.find.word);
            MatchQuery { pattern, regex, case: self.find.case }
        });
        let effects = self.controller.set_match_query(tab, query);
        self.perform(effects)
    }

    /// A window-only action the controller routed here (from `ced.action`
    /// over the Bus, or missing the args only a human can give): perform it
    /// as the window would, keeping the caller's intent for anything it
    /// starts (Save As).
    fn on_window_action(&mut self, tab: Option<TabId>, action: ActionId, args: Option<serde_json::Value>, intent: Intent) -> Task<Msg> {
        let mut selected = Task::none();
        if let Some(t) = tab
            && Some(t) != self.controller.active()
        {
            let effects = self.controller.select_tab(t);
            selected = self.perform(effects);
        }
        let arg = |k: &str| args.as_ref().and_then(|a| a.get(k)).and_then(|v| v.as_str()).map(str::to_owned);
        let task = match action {
            ActionId::FileSaveAs => match self.controller.active() {
                Some(t) => {
                    let name = self.tab(t).map(chrome::tabs::display_name);
                    self.open_dialog(FileMode::SaveAs { tab: t, intent }, name)
                }
                None => Task::none(),
            },
            ActionId::SearchFind | ActionId::SearchFindNext | ActionId::SearchFindPrev | ActionId::SearchReplace | ActionId::SearchReplaceAll => {
                let replace = matches!(action, ActionId::SearchReplace | ActionId::SearchReplaceAll);
                let opened = self.on_ui_action(if replace { ActionId::SearchReplace } else { ActionId::SearchFind });
                // After opening: the caller's pattern beats the selection seed.
                if let Some(p) = arg("pattern") {
                    self.find.pattern = p;
                }
                if let Some(r) = arg("replacement") {
                    self.find.replacement = r;
                }
                opened
            }
            other => self.on_ui_action(other),
        };
        Task::batch([selected, task])
    }

    fn on_escape(&mut self) -> Task<Msg> {
        if self.modal.take().is_some() {
            return Task::none();
        }
        if self.find.open {
            self.find.open = false;
            self.field_focused = false;
            return self.push_match_query();
        }
        if self.panel.take().is_some() {
            return Task::none();
        }
        Task::none()
    }

    fn open_dialog(&mut self, mode: FileMode, name: Option<String>) -> Task<Msg> {
        let dir = self
            .active_tab()
            .and_then(|t| t.mirror.as_ref()?.meta().path.clone().or_else(|| t.path.clone()))
            .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf()))
            .filter(|d| d.is_dir())
            .or_else(|| std::env::current_dir().ok())
            .or_else(crate::chrome::dialogs::file::home)
            .unwrap_or_else(|| PathBuf::from("/"));
        let recent = self.recent();
        let mut dialog = FileDialog::new(mode, dir, recent);
        if let Some(name) = name {
            dialog = dialog.with_name(&name);
        }
        self.modal = Some(Modal::File(dialog));
        Task::batch([
            iced::widget::operation::focus(crate::chrome::dialogs::file::PATH_INPUT),
            iced::widget::operation::move_cursor_to_end(crate::chrome::dialogs::file::PATH_INPUT),
        ])
    }

    fn on_dialog(&mut self, msg: DialogMsg) -> Task<Msg> {
        match msg {
            DialogMsg::Close => {
                self.modal = None;
                Task::none()
            }
            DialogMsg::File(f) => {
                let Some(Modal::File(d)) = &mut self.modal else { return Task::none() };
                match d.update(f) {
                    None => Task::none(),
                    Some(FileOutcome::Open(paths)) => {
                        self.modal = None;
                        let intent = Intent::ui(self.controller.active().unwrap_or(0));
                        let effects = self.controller.open_paths(&paths, intent);
                        self.perform(effects)
                    }
                    Some(FileOutcome::SaveAs { tab, intent, path, exists }) => {
                        if exists {
                            self.modal = Some(Modal::Confirm(Confirm::Overwrite { tab, path, intent }));
                            return Task::none();
                        }
                        self.modal = None;
                        self.save_as(tab, intent, path, false)
                    }
                }
            }
            DialogMsg::Confirm(choice) => {
                let Some(Modal::Confirm(confirm)) = self.modal.take() else { return Task::none() };
                match (confirm, choice) {
                    (_, Choice::Cancel) => Task::none(),
                    (Confirm::Overwrite { tab, path, intent }, Choice::Accept) => self.save_as(tab, intent, path, true),
                    (Confirm::DiskModified { tab, intent, .. }, Choice::Accept) => {
                        let effects = self.controller.on_action_args(Some(tab), ActionId::FileSave, Some(serde_json::json!({"force": true})), intent);
                        self.perform(effects)
                    }
                    (Confirm::CloseDirty { tab, intent, .. }, Choice::Accept) => {
                        // Save, then close once the save lands (the controller
                        // closes after its save completes when asked to).
                        let is_scratch = self.tab(tab).is_some_and(|t| t.mirror.as_ref().is_some_and(|m| m.meta().path.is_none()));
                        if is_scratch {
                            let name = self.tab(tab).map(chrome::tabs::display_name);
                            return self.open_dialog(FileMode::SaveAs { tab, intent }, name);
                        }
                        let effects = self.controller.on_action_args(Some(tab), ActionId::FileClose, Some(serde_json::json!({"save": true})), intent);
                        self.perform(effects)
                    }
                    (Confirm::CloseDirty { tab, intent, .. }, Choice::Discard) => {
                        let effects = self.controller.on_action_args(Some(tab), ActionId::FileClose, Some(serde_json::json!({"force": true})), intent);
                        self.perform(effects)
                    }
                    (_, Choice::Discard) => Task::none(),
                }
            }
            DialogMsg::Goto(g) => {
                let Some(Modal::Goto(d)) = &mut self.modal else { return Task::none() };
                let Some((line, col)) = d.update(g) else { return Task::none() };
                self.modal = None;
                let offset = self.active_tab().and_then(|t| t.mirror.as_ref()).map(|m| line_col_offset(m.text(), line, col));
                offset.map_or_else(Task::none, |o| self.move_caret(o))
            }
            DialogMsg::Recovered(r) => {
                let Some(Modal::Recovered(d)) = &mut self.modal else { return Task::none() };
                let intent = Intent::ui(self.controller.active().unwrap_or(0));
                let (buffers, discard): (Vec<String>, bool) = match r {
                    RecoveredMsg::Open(b) => (vec![b], false),
                    RecoveredMsg::Discard(b) => (vec![b], true),
                    RecoveredMsg::OpenAll => (d.buffers.iter().map(|b| b.buffer.clone()).collect(), false),
                };
                let mut effects = Vec::new();
                let mut empty = false;
                for b in &buffers {
                    effects.extend(if discard {
                        self.controller.discard_recovered(b)
                    } else {
                        self.controller.open_recovered(b, intent.clone())
                    });
                    empty = d.handled(b);
                }
                if empty {
                    self.modal = None;
                }
                self.perform(effects)
            }
        }
    }

    fn save_as(&mut self, tab: TabId, intent: Intent, path: String, force: bool) -> Task<Msg> {
        let effects = self.controller.on_action_args(Some(tab), ActionId::FileSaveAs, Some(serde_json::json!({"path": path, "force": force})), intent);
        self.perform(effects)
    }

    fn on_info(&mut self, action: InfoAction) -> Task<Msg> {
        let intent = |tab: TabId| Intent::ui(tab);
        let effects = match action {
            InfoAction::Dismiss(key) => {
                if let InfoKey::Notice(id) = key {
                    self.notices.retain(|(n, ..)| *n != id);
                } else if let InfoKey::Conflict(tab, rev) = key {
                    self.controller.dismiss_conflict(tab, rev);
                } else {
                    self.dismissed.insert(key);
                }
                return Task::none();
            }
            InfoAction::Reload(tab) => self.controller.on_action(Some(tab), ActionId::FileReload, intent(tab)),
            InfoAction::Save(tab) => {
                let effects = self.controller.select_tab(tab);
                let selected = self.perform(effects);
                return Task::batch([selected, self.on_ui_action(ActionId::FileSave)]);
            }
            InfoAction::CloseTab(tab) => self.controller.on_action(Some(tab), ActionId::FileClose, intent(tab)),
            InfoAction::ShowConflict(tab, rev) => {
                if let Some(c) = self.conflict(tab, rev) {
                    self.modal = Some(Modal::Conflict(crate::chrome::dialogs::conflict::ConflictView { tab, conflict: c }));
                }
                return Task::none();
            }
            InfoAction::CopyConflict(tab, rev) => {
                return self.conflict(tab, rev).map_or_else(Task::none, |c| iced::clipboard::write(c.texts.concat()));
            }
            InfoAction::ReinsertConflict(tab, rev) => {
                let Some(c) = self.conflict(tab, rev) else { return Task::none() };
                self.modal = None;
                self.controller.dismiss_conflict(tab, rev);
                self.controller.on_editor(tab, EditorMsg::Command(EditCommand::Insert(c.texts.concat())))
            }
            InfoAction::KeepMine(tab) => self.controller.keep_mine(tab, intent(tab)),
            InfoAction::TakeService(tab) => self.controller.take_service(tab),
            InfoAction::SaveMineAs(tab) => {
                // The copy lands in a new, active scratch tab: offer Save As on it.
                let effects = self.controller.keep_as_new(tab, intent(tab));
                let copied = self.perform(effects);
                return Task::batch([copied, self.on_ui_action(ActionId::FileSaveAs)]);
            }
            InfoAction::KeepAsNew(tab) => self.controller.keep_as_new(tab, intent(tab)),
        };
        self.perform(effects)
    }

    fn conflict(&self, tab: TabId, rev: u64) -> Option<cosmix_edit_client::types::Conflict> {
        self.tab(tab)?.mirror.as_ref()?.conflicts().iter().find(|c| c.rev == rev).cloned()
    }

    fn on_prompt(&mut self, prompt: Prompt) {
        match prompt {
            Prompt::CloseDirty { tab, intent } => {
                let name = self.tab(tab).map(chrome::tabs::display_name).unwrap_or_default();
                self.modal = Some(Modal::Confirm(Confirm::CloseDirty { tab, name, intent }));
            }
            Prompt::DiskModified { tab, intent } => {
                let name = self.tab(tab).map(chrome::tabs::display_name).unwrap_or_default();
                self.modal = Some(Modal::Confirm(Confirm::DiskModified { tab, name, intent }));
            }
            Prompt::Recovered { buffers } => {
                if !buffers.is_empty() && self.modal.is_none() {
                    self.modal = Some(Modal::Recovered(Recovered { buffers }));
                }
            }
        }
    }

    fn on_notice(&mut self, tab: Option<TabId>, notice: Notice) {
        match notice {
            // Conflicts and detached copies are drawn from mirror state.
            Notice::Conflict(_) | Notice::DetachedCopy { .. } => {}
            Notice::Message { level: Level::Info, text } => {
                if self.find.open {
                    self.find.status = Some(text.clone());
                }
                self.status = Some(text);
                self.timers.arm(TimerKey::StatusExpiry, STATUS_MS);
            }
            Notice::Message { level, text } => self.post(tab, level, text),
        }
    }

    fn post(&mut self, tab: Option<TabId>, level: Level, text: String) {
        self.notice_seq += 1;
        self.notices.push((self.notice_seq, tab, level, text));
        // Keep the strip short: the oldest go first.
        if self.notices.len() > 4 {
            self.notices.remove(0);
        }
    }

    fn on_window(&mut self, event: iced::window::Event) -> Task<Msg> {
        match event {
            iced::window::Event::Resized(size) => self.window = size,
            iced::window::Event::Focused => {
                self.window_focused = true;
                if let Some(tab) = self.controller.active() {
                    self.timers.arm(TimerKey::ClearMarkers(tab), MARKER_CLEAR_MS);
                }
            }
            iced::window::Event::Unfocused => self.window_focused = false,
            iced::window::Event::FileDropped(path) => {
                let intent = Intent::ui(self.controller.active().unwrap_or(0));
                let effects = self.controller.open_paths(&[path.to_string_lossy().into_owned()], intent);
                return self.perform(effects);
            }
            iced::window::Event::CloseRequested => return self.quit(),
            _ => {}
        }
        Task::none()
    }

    /// Detach and exit (D13): save the session now, then close.
    fn quit(&mut self) -> Task<Msg> {
        if self.quitting {
            return Task::none();
        }
        self.quitting = true;
        self.save_session();
        iced::exit()
    }

    /// Work that follows any state change: agent-edit badges, marker timers,
    /// lint scheduling, a pending macro, stale dismissals.
    fn after_transition(&mut self) -> Task<Msg> {
        let active = self.controller.active();
        if active != self.last_active {
            self.last_active = active;
            if let Some(tab) = active {
                self.timers.arm(TimerKey::ClearMarkers(tab), MARKER_CLEAR_MS);
            }
            self.find.status = None;
        }
        let mut tasks = Vec::new();
        let mut lint_now = Vec::new();
        let ids: Vec<TabId> = self.controller.tabs().iter().map(|t| t.id).collect();
        self.lint.retain(|id, _| ids.contains(id));
        for tab in self.controller.tabs() {
            let Some(m) = tab.mirror.as_ref() else { continue };
            if !matches!(m.meta().disk, cosmix_edit_core::wire::DiskState::Modified) {
                self.dismissed.remove(&InfoKey::DiskModified(tab.id));
            }
            if !self.config.lint_on_save
                || !crate::lint::lints(&m.meta().language)
                || m.meta().path.is_none()
                || !matches!(m.phase(), cosmix_edit_client::mirror::Phase::Live)
            {
                continue;
            }
            let track = self.lint.entry(tab.id).or_default();
            let saved = Some(m.meta().saved_rev);
            if track.seen_gen.is_none() || track.seen_saved_rev != saved {
                // Opened, or saved: lint now.
                track.seen_gen = Some(m.view_gen());
                track.seen_saved_rev = saved;
                lint_now.push(tab.id);
            } else if track.seen_gen != Some(m.view_gen()) {
                track.seen_gen = Some(m.view_gen());
                if m.text().len() <= crate::lint::DEBOUNCE_MAX_BYTES {
                    self.timers.arm(TimerKey::Lint(tab.id), crate::lint::DEBOUNCE_MS);
                }
            }
        }
        for tab in lint_now {
            tasks.push(self.start_lint(tab));
        }
        if let Some((stem, tab)) = self.macro_pending.clone()
            && self.tab(tab).and_then(|t| t.mirror.as_ref()).is_some_and(|m| m.is_idle())
        {
            self.macro_pending = None;
            tasks.push(self.start_macro(&stem, tab));
        }
        Task::batch(tasks)
    }

    fn start_lint(&mut self, tab: TabId) -> Task<Msg> {
        let Some(track) = self.lint.get_mut(&tab) else { return Task::none() };
        if track.inflight {
            // One lint per tab at a time; the one running will be followed by
            // the debounce the next edit arms.
            self.timers.arm(TimerKey::Lint(tab), crate::lint::DEBOUNCE_MS);
            return Task::none();
        }
        let Some((tag, text, cwd)) = self.controller.lint_capture(tab, crate::lint::cfg_hash()) else { return Task::none() };
        track.inflight = true;
        Task::perform(crate::lint::spawn(tag, text, cwd), move |(tag, result)| Msg::Lint(tab, tag, result))
    }

    // ── macros ──────────────────────────────────────────────────────────────

    fn reload_macros(&mut self) {
        let (defs, notes) = match &self.dirs {
            Some(d) => macros::discover_with_notes(&d.macros_dir()),
            None => (Vec::new(), Vec::new()),
        };
        self.bindings = Bindings::new(defs.iter().filter_map(|m| Some((m.stem.as_str(), m.chord.as_deref()?))));
        self.macros = defs;
        for note in notes {
            self.output.push(note, true);
        }
    }

    fn run_macro(&mut self, stem: String) -> Task<Msg> {
        let Some(tab) = self.controller.active() else { return Task::none() };
        if self.macro_running.is_some() {
            self.status = Some("A macro is already running".into());
            return Task::none();
        }
        let idle = self.tab(tab).and_then(|t| t.mirror.as_ref()).is_some_and(|m| m.is_idle());
        if idle {
            self.start_macro(&stem, tab)
        } else {
            self.macro_pending = Some((stem, tab));
            Task::none()
        }
    }

    fn start_macro(&mut self, stem: &str, tab: TabId) -> Task<Msg> {
        let Some(def) = self.macros.iter().find(|m| m.stem == stem).cloned() else { return Task::none() };
        let Some(t) = self.tab(tab) else { return Task::none() };
        let Some(m) = t.mirror.as_ref() else { return Task::none() };
        let sel = t.editor.sel;
        let env = MacroEnv {
            buffer: m.buffer().to_owned(),
            epoch: m.epoch().to_owned(),
            rev: m.rev(),
            path: m.meta().path.clone(),
            language: m.meta().language.clone(),
            sel_start: sel.anchor.min(sel.head),
            sel_end: sel.anchor.max(sel.head),
        };
        self.macro_running = Some(def.stem.clone());
        self.panel = Some(Panel::Output);
        self.output.push(format!("▶ {} ({})", def.label, def.origin()), false);
        Task::run(macros::spawn(&def, &env), Msg::Macro)
    }

    fn on_macro(&mut self, event: MacroEvent) -> Task<Msg> {
        match event {
            MacroEvent::Line { text, stderr, .. } => self.output.push(text, stderr),
            MacroEvent::Exit { stem, code } => {
                self.macro_running = None;
                let ok = code == Some(0);
                self.output.push(format!("■ {stem} exited {}", code.map_or("by signal".into(), |c| c.to_string())), !ok);
            }
            MacroEvent::Failed { stem, error } => {
                self.macro_running = None;
                self.output.push(format!("■ {stem}: {error}"), true);
                self.post(None, Level::Error, format!("Macro {stem}: {error}"));
            }
        }
        iced::widget::operation::snap_to_end(crate::chrome::output::SCROLL_ID)
    }

    // ── settings, theme, session ────────────────────────────────────────────

    fn reload_settings(&mut self) {
        if let Some(d) = &self.dirs {
            let (config, note) = config::load(&d.config_file());
            self.whitespace = config.show_whitespace;
            self.line_numbers = config.line_numbers;
            self.remote_carets = config.remote_carets;
            self.config = config;
            if let Some(note) = note {
                self.post(None, Level::Warn, note);
            }
        }
        self.reload_macros();
        self.reload_theme();
        self.status = Some("Settings reloaded".into());
        self.timers.arm(TimerKey::StatusExpiry, STATUS_MS);
    }

    fn reload_theme(&mut self) {
        self.theme = theme::resolve(self.dirs.as_ref().map(|d| d.theme_override()).as_deref());
        if let Some(note) = self.theme.notes.clone() {
            self.post(None, Level::Warn, format!("Theme: {note}"));
        }
    }

    fn save_session(&mut self) {
        let Some(path) = self.dirs.as_ref().map(|d| d.session_file()) else { return };
        let session = self.controller.session();
        if let Err(e) = crate::session::save(&path, &session) {
            tracing::warn!("ced: saving {}: {e}", path.display());
        }
    }

    fn recent(&self) -> Vec<String> {
        self.controller.session().recent
    }

    fn selection_text(&self, max: usize) -> Option<String> {
        let tab = self.active_tab()?;
        let m = tab.mirror.as_ref()?;
        let (a, b) = (tab.editor.sel.anchor.min(tab.editor.sel.head), tab.editor.sel.anchor.max(tab.editor.sel.head));
        if a == b || b - a > max {
            return None;
        }
        let mut s = String::new();
        m.text().read(a..b, &mut s);
        Some(s)
    }

    fn base_px(&self) -> f32 {
        self.config.font_px.map_or(self.theme.mono.1, f32::from)
    }

    fn text_px(&self) -> f32 {
        self.zoom_px.map_or_else(|| self.base_px(), f32::from)
    }

    fn unprotected(&self) -> bool {
        self.controller.edit_info().is_some_and(|i| i.volatile != Some(false))
    }

    fn layout_reply(&self, tab: TabId, r: LayoutReport) -> LayoutReply {
        let rect = |v: [f32; 4]| Rect { x: v[0], y: v[1], w: v[2], h: v[3] };
        let (w, h) = (self.window.width, self.window.height);
        LayoutReply {
            tab,
            window: Rect { x: 0.0, y: 0.0, w, h },
            menubar: Rect { x: 0.0, y: 0.0, w, h: MENU_H },
            tabstrip: Rect { x: 0.0, y: MENU_H, w, h: TABS_H },
            editor: rect(r.editor),
            gutter_w: r.gutter_w,
            line_height: r.line_height,
            cell_w: r.cell_w,
            first_line: r.first_line,
            visible_rows: r.visible_rows,
            caret: rect(r.caret),
            statusbar: Rect { x: 0.0, y: h - STATUS_H, w, h: STATUS_H },
        }
    }

    // ── view ────────────────────────────────────────────────────────────────

    fn subscription(&self) -> Subscription<Msg> {
        let mut subs = vec![
            Subscription::run(streams),
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Window(e @ (iced::window::Event::Resized(_)
                | iced::window::Event::Focused
                | iced::window::Event::Unfocused
                | iced::window::Event::FileDropped(_)
                | iced::window::Event::CloseRequested)) => Some(Msg::Window(e)),
                _ => None,
            }),
        ];
        // Frames only while something waits for one: a key's next-frame
        // measurement, or a highlighter catching up on a cold seek.
        let behind = self.active_tab().is_some_and(|t| t.highlight.behind());
        if self.key_at.is_some() || behind {
            subs.push(iced::window::frames().map(Msg::Frame));
        }
        Subscription::batch(subs)
    }

    fn look(&self) -> Look {
        Look::new(&self.theme, self.text_px())
    }

    fn view(&self) -> Element<'_, Msg> {
        let started = Instant::now();
        let element = self.view_inner();
        self.view_us.set(started.elapsed().as_micros() as u64);
        element
    }

    fn view_inner(&self) -> Element<'_, Msg> {
        let look = self.look();
        let tabs = self.controller.tabs();
        let active = self.controller.active();
        let tab = self.active_tab();

        let menu_ctx = self.menu_ctx();
        let menubar = cosmix_iced_widgets::Menu::bar(chrome::menu::bar(&menu_ctx))
            .id(BAR_ID)
            .style(cosmix_iced_widgets::MenuStyle { text_size: look.ui_px, row_height: MENU_H, ..look.tokens.menu_style() });

        let mut body = column![menubar, chrome::tabs::view(look, tabs, active, |id| self.controller.agent_since_focus(id))];
        let infos = self.infos(tab);
        if !infos.is_empty() {
            body = body.push(chrome::infobar::view(look, infos));
        }
        body = body.push(self.editor_area(look, tab));
        if self.find.open {
            body = body.push(keys::focus_probe(chrome::find::view(look, &self.find), Msg::FindFocused));
        }
        match self.panel {
            Some(Panel::Problems) => {
                let items = tab.map(|t| t.diagnostics.items()).unwrap_or(&[]);
                let note = tab.and_then(|t| self.lint.get(&t.id)?.note.as_deref());
                let col_of = |offset: usize| tab.and_then(|t| t.mirror.as_ref()).map_or(1, |m| m.text().point(offset).col);
                body = body.push(chrome::problems::view(look, items, col_of, note));
            }
            Some(Panel::Output) => body = body.push(chrome::output::view(look, &self.output)),
            None => {}
        }
        body = body.push(chrome::status::view(look, tab, self.unprotected(), self.status.as_deref()));

        let modal_open = self.modal.is_some();
        let base: Element<'_, Msg> = if modal_open { keys::inert(body).into() } else { body.into() };
        let overlay: Element<'_, Msg> = match &self.modal {
            Some(modal) => modal.view(look, &self.dialog_ctx()),
            None => iced::widget::space().into(),
        };
        let routed = keys::router(stack![base, overlay], &self.bindings, route_msg)
            .modal(modal_open)
            .text_field(self.field_focused && self.find.open)
            .on_zoom(Msg::Zoom)
            .on_unclaimed(move |key, mods| unclaimed(key, mods, modal_open));
        container(routed).width(Length::Fill).height(Length::Fill).into()
    }

    fn editor_area<'a>(&'a self, look: Look, tab: Option<&'a Tab>) -> Element<'a, Msg> {
        let Some(tab) = tab else {
            return container(
                column![
                    look.text("No file open").size(look.ui_px * 1.3).color(look.tokens.muted_text),
                    look.small("Ctrl+O opens a file · Ctrl+N starts a new buffer").color(look.tokens.muted_text),
                ]
                .spacing(8)
                .align_x(iced::Alignment::Center),
            )
            .center(Length::Fill)
            .style(look.strip(self.theme.palette.background, self.theme.palette.text))
            .into();
        };
        let Some(m) = tab.mirror.as_ref() else {
            return container(look.text(format!("Opening {}…", tab.path.as_deref().unwrap_or("buffer"))).color(look.tokens.muted_text))
                .center(Length::Fill)
                .style(look.strip(self.theme.palette.background, self.theme.palette.text))
                .into();
        };
        let view = EditorView {
            font: self.theme.mono_font,
            px: self.text_px(),
            line_height: 1.35,
            measure: cosmix_edit_core::view::MeasureCfg { tab_size: self.config.tab_size, ambiguous_wide: self.config.ambiguous_wide },
            whitespace: self.whitespace,
            line_numbers: self.line_numbers,
            remote_carets: self.remote_carets,
            focused: self.window_focused && self.modal.is_none() && !(self.find.open && self.field_focused),
        };
        let id = tab.id;
        let diagnostics = if self.config.lint_on_save { &tab.diagnostics } else { &self.empty_diag };
        EditorWidget::with(m.text(), &tab.editor, &tab.highlight, &self.theme.palette, diagnostics, &view)
            .map(move |msg| Msg::Editor(id, msg))
    }

    fn infos(&self, tab: Option<&Tab>) -> Vec<Info> {
        let dismissed = |k: &InfoKey| self.dismissed.contains(k);
        let mut infos = tab.map(|t| chrome::infobar::for_tab(t, &dismissed)).unwrap_or_default();
        if self.unprotected() && !dismissed(&InfoKey::Unprotected) {
            infos.push(Info {
                key: InfoKey::Unprotected,
                level: Level::Warn,
                text: "The edit service is not protecting unsaved text (recovery off or degraded): a crash can lose it.".into(),
                actions: vec![("Dismiss".into(), InfoAction::Dismiss(InfoKey::Unprotected))],
            });
        }
        for (id, t, level, text) in &self.notices {
            if t.is_none() || *t == tab.map(|t| t.id) {
                infos.push(Info { key: InfoKey::Notice(*id), level: *level, text: text.clone(), actions: Vec::new() });
            }
        }
        infos
    }

    fn menu_ctx(&self) -> MenuCtx<'_> {
        let tab = self.active_tab();
        let m = tab.and_then(|t| t.mirror.as_ref());
        let sel = tab.map(|t| t.editor.sel);
        MenuCtx {
            has_tab: tab.is_some(),
            live: m.is_some_and(|m| matches!(m.phase(), cosmix_edit_client::mirror::Phase::Live)),
            dirty: m.is_some_and(|m| m.meta().dirty),
            any_dirty: self.controller.tabs().iter().any(|t| t.mirror.as_ref().is_some_and(|m| m.meta().dirty)),
            has_path: m.is_some_and(|m| m.meta().path.is_some()),
            has_selection: sel.is_some_and(|s| s.anchor != s.head),
            other_lane: m.and_then(|m| m.last_remote()).map(|r| r.lane.as_str()),
            find_active: !self.find.pattern.is_empty(),
            overwrite: tab.is_some_and(|t| t.editor.overwrite),
            whitespace: self.whitespace,
            line_numbers: self.line_numbers,
            remote_carets: self.remote_carets,
            problems: self.panel == Some(Panel::Problems),
            output: self.panel == Some(Panel::Output),
            macros: &self.macros,
            macro_running: self.macro_running.is_some(),
            tabs: self.controller.tabs().iter().map(|t| (t.id, chrome::tabs::display_name(t))).collect(),
            active: self.controller.active(),
        }
    }

    fn dialog_ctx(&self) -> DialogCtx<'_> {
        let info = self.controller.edit_info();
        DialogCtx {
            edit_version: info.and_then(|i| i.version.as_deref()),
            edit_epoch: info.and_then(|i| i.epoch.as_deref()),
            volatile: info.and_then(|i| i.volatile),
            theme: format!("{} · {}", self.theme.scheme.name(), self.theme.mode.name()),
            mono: format!("{} {} px", self.theme.mono.0, self.text_px()),
            ui: format!("{} {} px", self.theme.ui.0, self.theme.ui.1),
            config_path: self.dirs.as_ref().map(|d| d.config_file().display().to_string()),
            macros: &self.macros,
        }
    }
}

/// A Mix relex (`Effect::Relex`) on its own thread, so a large buffer never
/// stalls the UI thread or the one-thread task pool.
fn relex(
    tag: cosmix_edit_client::highlight::ResultTag,
    source: std::sync::Arc<str>,
) -> impl std::future::Future<
    Output = (cosmix_edit_client::highlight::ResultTag, Vec<(std::ops::Range<usize>, cosmix_edit_client::highlight::TokenClass)>),
> + Send
+ 'static {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    let language = tag.language.clone();
    let spawned = std::thread::Builder::new().name("ced-relex".into()).spawn(move || {
        let _ = tx.send(cosmix_edit_client::highlight::run_mix(&language, &source));
    });
    async move {
        let spans = match spawned {
            Ok(_) => rx.await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        (tag, spans)
    }
}

fn route_msg(routed: Routed) -> Msg {
    match routed {
        Routed::OpenMenu(index) => Msg::OpenMenu(index),
        Routed::Run(Binding::Action(action)) => Msg::Action(action),
        Routed::Run(Binding::Macro(stem)) => Msg::RunMacro(stem),
    }
}

/// Keys no widget took: Escape closes things; in a dialog Tab completes the
/// path and the arrows move through the file list.
fn unclaimed(key: &Key, mods: iced::keyboard::Modifiers, modal: bool) -> Option<Msg> {
    match key {
        Key::Named(Named::Escape) => Some(Msg::Escape),
        Key::Named(Named::Tab) if modal && !mods.shift() && !mods.control() => Some(Msg::FileTab),
        Key::Named(n @ (Named::ArrowUp | Named::ArrowDown)) if modal => Some(Msg::DialogKey(*n)),
        _ => None,
    }
}

/// View byte offset of 1-based `line` and editd `col` (scalars, 1-based;
/// past the end clamps to the line end).
pub fn line_col_offset(text: &cosmix_edit_core::text::Text, line: usize, col: Option<usize>) -> usize {
    let Some(range) = text.line_range(line.clamp(1, text.line_count().max(1))) else { return 0 };
    let Some(col) = col.filter(|c| *c > 1) else { return range.start };
    let mut s = String::new();
    text.read(range.clone(), &mut s);
    let within = s.char_indices().nth(col - 1).map_or(s.len(), |(i, _)| i);
    range.start + within
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::Menu as MenuName;

    #[test]
    fn goto_offsets_count_scalars() {
        let text = cosmix_edit_core::text::Text::from_text("one\nzwölf x\nlast").unwrap();
        assert_eq!(line_col_offset(&text, 1, None), 0);
        assert_eq!(line_col_offset(&text, 2, Some(5)), 4 + "zwöl".len());
        assert_eq!(line_col_offset(&text, 2, Some(99)), 4 + "zwölf x".len());
        assert_eq!(line_col_offset(&text, 9, None), "one\nzwölf x\n".len(), "past the end clamps to the last line");
    }

    #[test]
    fn unclaimed_keys() {
        let none = iced::keyboard::Modifiers::empty();
        assert_eq!(unclaimed(&Key::Named(Named::Escape), none, false), Some(Msg::Escape));
        assert_eq!(unclaimed(&Key::Named(Named::Tab), none, true), Some(Msg::FileTab));
        assert_eq!(unclaimed(&Key::Named(Named::Tab), none, false), None, "outside a dialog Tab belongs to the editor");
    }

    #[test]
    fn menus_route_by_mnemonic_order() {
        assert_eq!(route_msg(Routed::OpenMenu(2)), Msg::OpenMenu(2));
        assert_eq!(MenuName::ALL[2], MenuName::Search);
    }
}
