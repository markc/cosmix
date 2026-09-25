//! The Controller (ced E1 plan §2): tabs, the global `event_seq`, the op-id
//! generator, `ced.*` verbs, `ced.wait` waiters, reattach. iced-free — the GUI
//! (`app.rs`) and `--headless` (`headless.rs`) drive the same Controller.
//! Stage S froze the signatures; Stage E1d implements them.
//!
//! Contracts:
//! - Every `edit.changed` event is routed to its buffer's mirror; the global
//!   `last_event_seq` (initialised from the first event after (re)subscribe)
//!   detects gaps → every Live mirror `suspect()`s; so do `resync all` and the
//!   Bus reconnect edge. An epoch change (event/reply epoch, `epoch_mismatch`,
//!   `edit` reappearing in noded `services.registered`) → reattach every tab
//!   (plan §3.8).
//! - Bus-driven actions carry `Intent::bus(tab, caller_key)`; window input
//!   carries `Intent::ui(tab)`; the intent travels through every async
//!   completion (paste, dialogs, find/replace).
//! - `ced.wait` (plan §4.8): evaluated at registration and after EVERY
//!   controller transition; a one-shot deadline timer; cancelled on tab close
//!   / Bus loss. Never a loop.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::highlight::Highlight;
use cosmix_edit_client::mirror::{DetachReason, LaneArg, Mirror, Outcome, Phase, ServerOp, Step};
use cosmix_edit_client::model::{EditCfg, EditCommand, EditorModel};
use cosmix_edit_client::types::{DeltaKind, Incoming, Intent, Invoker, Level, Notice, OpIdGen, Outgoing, TabId, UI_ORIGIN};
use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::pos::{NamedPos, PosSpec};
use cosmix_edit_core::text::Text;
use cosmix_edit_core::view::MeasureCfg;
use cosmix_edit_core::wire;
use serde::Serialize;
use serde_json::{Value, json};

use crate::actions::ActionId;
use crate::config::Config;
use crate::editor::{EditorMsg, LayoutReport};
use crate::session::{RECENT_MAX, Session, SessionTab};
use crate::verbs::{self, PhaseW, code};

mod ui;

pub use ui::{MatchQuery, Prompt, RecoveredRow};

/// One tab = one buffer view.
pub struct Tab {
    pub id: TabId,
    /// `None` until `edit.open` answers (or after an open failure).
    pub mirror: Option<Mirror>,
    pub editor: EditorModel,
    pub highlight: Highlight,
    pub diagnostics: Diagnostics,
    /// Path given by the user (kept across reattach).
    pub path: Option<String>,
}

/// A `ced.*` Bus request, as the bus thread hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusCommand {
    /// Correlation for the reply (the bus thread answers `respond(id, …)`).
    pub id: u64,
    pub verb: String,
    /// JSON body.
    pub body: String,
    /// Attested caller key: `local:<from>`, `mesh:<service>@<peer>`, `anon`.
    pub caller_key: String,
}

/// What the Controller asks its host (GUI or headless loop) to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send a request; `req` correlates the eventual `Incoming::Reply`.
    Send { req: u64, out: Outgoing },
    /// Answer a `ced.*` Bus command.
    Respond { id: u64, rc: u8, body: String },
    /// Arm a one-shot timer (`Incoming::Timer{id}` after `ms`).
    Timer { id: u64, ms: u64 },
    /// Subscribe a topic (idempotent).
    Subscribe { topic: String },
    /// Show something in the chrome.
    Notice { tab: Option<TabId>, notice: Notice },
    /// Write the clipboard (`primary`: the selection clipboard).
    ClipboardWrite { text: String, primary: bool },
    /// Read the clipboard; the result comes back as `on_paste` with `intent`.
    ClipboardRead { primary: bool, intent: Intent },
    /// Persist the session (debounced by the host).
    SaveSession,
    /// Detach and exit (plan D13).
    Quit,
    /// Ask the human (a dialog); the answer comes back as an action or a
    /// [`Controller`] method (see [`Prompt`]). Added in E1d for E1f.
    Prompt(Prompt),
    /// A window-only action (dialogs, find bar, zoom, panels, help, and the
    /// args-taking actions called without their args): the window performs it
    /// and then calls [`Controller::ui_done`] with `token` and the outcome.
    /// A Bus `ced.action` that caused it is answered only from `ui_done` (or
    /// with a `TIMEOUT` refusal after 10 s) — never before the window acted.
    /// Never emitted headless (those refuse UNAVAILABLE).
    UiAction { tab: Option<TabId>, action: ActionId, args: Option<Value>, intent: Intent, token: u64 },
    /// Run a Mix relex off the UI thread:
    /// `cosmix_edit_client::highlight::run_mix(&tag.language, &source)`, then
    /// hand the spans to [`Controller::on_relex`] with the same tag.
    Relex { tab: TabId, tag: cosmix_edit_client::highlight::ResultTag, source: std::sync::Arc<str> },
}

/// `ced.wait` longest deadline.
const WAIT_MAX_MS: u64 = 30_000;
/// `ced.state` refuses to inline more text than this.
const STATE_TEXT_MAX: usize = 4 * 1024 * 1024;
/// Selection publish: 200 ms after the last caret move, at most one per second.
const PUBLISH_DEBOUNCE_MS: u64 = 200;
const PUBLISH_MIN_GAP_MS: u64 = 1_000;
/// Session writes: 1 s after the last change.
const SESSION_DEBOUNCE_MS: u64 = 1_000;
/// Deadlines for the controller's own requests (plan §2).
const DEADLINE_MS: u64 = 5_000;
const DEADLINE_LONG_MS: u64 = 30_000;
/// A Bus `ced.action` waits this long for its server op's outcome (a save's
/// own deadline, its `edit.list` check and one resend fit inside).
const OP_WAIT_MS: u64 = 90_000;
/// `edit.open` deadlines before the tab says the service is silent.
const OPEN_WARN_AFTER: u32 = 3;

/// Controller-private per-tab state.
#[derive(Default)]
struct TabX {
    recovery_id: Option<String>,
    /// `edit.open` answered (success or failure) at least once.
    opened: bool,
    /// Waiting for a reattach open/list.
    reattaching: bool,
    open_error: Option<String>,
    /// `edit.open` deadlines passed in a row (GLM M5: the human hears once).
    open_misses: u32,
    /// The caret/selection last published with `edit.select`.
    published: Option<Selection>,
    last_publish: Option<Instant>,
    publish_timer: bool,
    /// Another origin edited since the tab was last focused.
    agent_since_focus: bool,
    layout: Option<LayoutReport>,
    /// Pending `line[:col]` from `path:line:col` or `ced.open {line, col}`.
    goto: Option<(usize, usize)>,
    /// `keep_as_new`: text to insert (by this intent) once the buffer is live.
    fill: Option<(String, Intent)>,
    /// `file.close {save:true}`: close once the save has landed.
    close_after_save: Option<Intent>,
    /// Who asked for the last save (a `disk_modified` refusal prompts them).
    save_intent: Option<Intent>,
    /// A find / replace waiting for the pipeline to go idle.
    find: Option<ui::FindJob>,
    /// An outstanding lint capture and the deltas since (plan §4.10).
    lint: Option<ui::LintCapture>,
    relex_timer: bool,
    /// Highlight-all: the query, its matches (view ranges), refresh state.
    match_query: Option<ui::MatchQuery>,
    matches: Vec<Range<usize>>,
    rematch_due: bool,
    rematch_timer: bool,
}

/// What a request the controller sent is for.
enum Req {
    Mirror { tab: TabId, op_id: String },
    Open { tab: TabId, reattach: bool },
    /// Scratch-tab reattach: find the buffer by `recovery_id`.
    List { tab: TabId },
    Info,
    Close { tab: TabId, cmd: Option<(u64, String)>, intent: Intent },
    Select,
    Find { tab: TabId, job: Box<ui::FindJob> },
    Matches { tab: TabId, query: ui::MatchQuery },
    /// `edit.list` at start for the recovered-buffers prompt.
    Recovered,
    /// `edit.list` for the tab's save state (Opus m2).
    Saved { tab: TabId },
    Discard,
}

enum TimerFor {
    Retry(TabId),
    Wait(u64),
    Publish(TabId),
    Session,
    /// Mix relex debounce (150 ms after the last delta).
    Relex(TabId),
    /// Highlight-all refresh debounce (100 ms after the last delta).
    Rematch(TabId),
    /// Change markers clear 2 s after the tab gains focus (plan §4.5).
    ClearMarkers(TabId),
    /// A Bus `ced.action` waiting on the window's `ui_done`.
    UiDeadline(u64),
    /// An `rc 0` reply whose effect has not arrived (the mirror's echo wait).
    Echo(TabId, String),
    /// A Bus `ced.action` waiting on its server op's outcome (by op id).
    OpWait(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitCond {
    Rev(u64),
    Idle,
    Epoch(String),
    Phase(PhaseW),
}

struct Waiter {
    id: u64,
    cmd: u64,
    tab: TabId,
    cond: WaitCond,
    since: Instant,
    timeout_ms: u64,
}

/// A `ced.open` / `ced.new` answered once every tab it opened has an answer.
struct PendingOpen {
    cmd: u64,
    tabs: Vec<TabId>,
    new: bool,
}

#[derive(Default)]
struct Stats {
    keys: u64,
    events: u64,
    history_recoveries: u64,
    snapshot_recoveries: u64,
    conflicts: u64,
    retries: u64,
    uncertain: u64,
}

pub struct Controller {
    config: Config,
    headless: bool,
    ids: OpIdGen,
    tabs: Vec<Tab>,
    x: HashMap<TabId, TabX>,
    active: Option<TabId>,
    next_tab: TabId,
    next_req: u64,
    next_timer: u64,
    next_waiter: u64,
    reqs: HashMap<u64, Req>,
    timers: HashMap<u64, TimerFor>,
    last_event_seq: Option<u64>,
    edit_epoch: Option<String>,
    edit_info: verbs::EditInfo,
    waiters: Vec<Waiter>,
    opens: Vec<PendingOpen>,
    recent: Vec<String>,
    /// Tabs to reattach at `start` (from session.json).
    restore: Option<Session>,
    session_timer: bool,
    config_path: Option<String>,
    session_path: Option<String>,
    stats: Stats,
    /// `set_layout`: the last frame's geometry (`ced.layout`).
    layout: Option<verbs::LayoutReply>,
    frames: ui::Frames,
    /// Recovered buffers offered at start, and their epoch.
    recovered: Vec<wire::BufferSummary>,
    recovered_epoch: String,
    /// `UiAction` tokens: the next one, and the Bus commands awaiting `ui_done`.
    next_ui_token: u64,
    ui_pending: HashMap<u64, (u64, String)>,
    /// Bus `ced.action`s answered from their server op's outcome, by op id
    /// (Opus m4), and the one being dispatched right now.
    op_waits: HashMap<String, (u64, String)>,
    op_cmd: Option<(u64, String)>,
}

fn refusal(code: &str, message: impl Into<String>, reason: Option<&str>) -> String {
    serde_json::to_string(&verbs::Refusal {
        error_code: code.to_string(),
        message: message.into(),
        reason: reason.map(str::to_string),
    })
    .unwrap_or_default()
}

fn ok_body<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
}

/// `failed`: the tab's open was refused (no mirror) — detached.
fn phase_w(m: Option<&Mirror>, failed: bool) -> PhaseW {
    match m.map(Mirror::phase) {
        Some(Phase::Live) => PhaseW::Live,
        Some(Phase::Bootstrapping { .. }) => PhaseW::Bootstrapping,
        Some(Phase::Recovering { .. }) => PhaseW::Recovering,
        Some(Phase::Detached { .. }) => PhaseW::Detached,
        None if failed => PhaseW::Detached,
        None => PhaseW::Bootstrapping,
    }
}

fn text_string(t: &Text) -> String {
    let mut s = String::with_capacity(t.len());
    t.read(0..t.len(), &mut s);
    s
}

fn disk_str(d: wire::DiskState) -> String {
    serde_json::to_value(d).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn kind_str(k: wire::KindW) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// A Bus reply body as the mirror wants it: rc 0 → the JSON body; a
/// refusal → the `{error_code, message, reason, …}` body.
fn reply_result(rc: u8, body: &str) -> Result<Value, wire::Refusal> {
    if rc < 10 {
        return Ok(if body.trim().is_empty() { Value::Null } else { serde_json::from_str(body).unwrap_or(Value::Null) });
    }
    Err(serde_json::from_str::<wire::Refusal>(body).unwrap_or_else(|_| wire::Refusal {
        error_code: wire::ErrorCode::Internal,
        message: body.to_string(),
        reason: None,
        buffer: None,
        rev: None,
        context: Default::default(),
    }))
}

/// An `edit.open`-shaped reply for a buffer known from `edit.list` (scratch
/// reattach, recovered buffers): the list row carries everything the mirror
/// needs except eol/bom, which a snapshot does not depend on.
fn open_reply_of(epoch: &str, b: &wire::BufferSummary) -> wire::OpenReply {
    wire::OpenReply {
        buffer: b.buffer.clone(),
        epoch: epoch.to_string(),
        path: b.path.clone(),
        opened_as: b.opened_as.clone(),
        name: b.name.clone(),
        language: b.language.clone(),
        rev: b.rev,
        lines: b.lines,
        bytes: b.bytes,
        eol: wire::Eol::Lf,
        bom: false,
        disk: b.disk,
        reopened: true,
        created: false,
        recovery_id: b.recovery_id.clone(),
        recovered: b.recovered,
        recovered_from: None,
    }
}

/// Resolve an editd POS on the view text (byte offset, `{line, col}` with
/// editd's col = scalars incl. `\r`, 1-based, or `start`/`end`).
fn resolve_pos(text: &Text, p: &PosSpec) -> Result<usize, String> {
    match p {
        PosSpec::Offset(o) => {
            if *o > text.len() || !text.is_char_boundary(*o) {
                return Err(format!("offset {o} is outside the text or not on a char boundary"));
            }
            Ok(*o)
        }
        PosSpec::Named(NamedPos::Start) => Ok(0),
        PosSpec::Named(NamedPos::End) => Ok(text.len()),
        PosSpec::LineCol { line, col } => offset_of_line_col(text, *line, col.unwrap_or(1)),
        PosSpec::Anchor { .. } => Err("anchors are not supported by ced.select".into()),
    }
}

fn offset_of_line_col(text: &Text, line: usize, col: usize) -> Result<usize, String> {
    let r = text.line_range(line).ok_or_else(|| format!("line {line} is outside 1..={}", text.line_count()))?;
    if col == 0 {
        return Err("col is 1-based".into());
    }
    let mut s = String::new();
    text.read(r.clone(), &mut s);
    let mut chars = s.char_indices().map(|(i, _)| i).chain(std::iter::once(s.len()));
    chars.nth(col - 1).map(|i| r.start + i).ok_or_else(|| format!("col {col} is past the end of line {line}"))
}

/// `path:line[:col]` → (path, line, col) when the suffix parses.
fn split_line_col(p: &str) -> (String, Option<(usize, usize)>) {
    let mut parts = p.rsplitn(3, ':');
    let last = parts.next();
    let mid = parts.next();
    let head = parts.next();
    match (head, mid, last) {
        (Some(h), Some(l), Some(c)) if !h.is_empty() => {
            if let (Ok(l), Ok(c)) = (l.parse::<usize>(), c.parse::<usize>()) {
                return (h.to_string(), Some((l, c)));
            }
            if let Ok(line) = c.parse::<usize>() {
                return (format!("{h}:{l}"), Some((line, 1)));
            }
            (p.to_string(), None)
        }
        (None, Some(h), Some(l)) if !h.is_empty() => match l.parse::<usize>() {
            Ok(line) => (h.to_string(), Some((line, 1))),
            Err(_) => (p.to_string(), None),
        },
        _ => (p.to_string(), None),
    }
}

impl Controller {
    /// `run_id`: random per process start (op ids); `headless`: no window.
    pub fn new(config: Config, run_id: u32, headless: bool) -> Self {
        Self {
            config,
            headless,
            ids: OpIdGen::new(run_id),
            tabs: Vec::new(),
            x: HashMap::new(),
            active: None,
            next_tab: 1,
            next_req: 0,
            next_timer: 0,
            next_waiter: 0,
            reqs: HashMap::new(),
            timers: HashMap::new(),
            last_event_seq: None,
            edit_epoch: None,
            edit_info: verbs::EditInfo { epoch: None, version: None, volatile: None },
            waiters: Vec::new(),
            opens: Vec::new(),
            recent: Vec::new(),
            restore: None,
            session_timer: false,
            config_path: None,
            session_path: None,
            stats: Stats::default(),
            layout: None,
            frames: ui::Frames::default(),
            recovered: Vec::new(),
            recovered_epoch: String::new(),
            next_ui_token: 0,
            ui_pending: HashMap::new(),
            op_waits: HashMap::new(),
            op_cmd: None,
        }
    }

    /// Tabs to reopen at [`Controller::start`] (the host loads session.json).
    pub fn set_session(&mut self, session: Session) {
        self.recent = session.recent.clone();
        self.restore = Some(session);
    }

    /// Paths reported by `ced.info`.
    pub fn set_paths(&mut self, config_path: Option<String>, session_path: Option<String>) {
        self.config_path = config_path;
        self.session_path = session_path;
    }

    /// The session to persist now.
    pub fn session(&self) -> Session {
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .map(|t| SessionTab {
                path: t.path.clone(),
                recovery_id: if t.path.is_none() { self.x.get(&t.id).and_then(|x| x.recovery_id.clone()) } else { None },
                caret: t.editor.sel.head,
                first_line: t.editor.scroll.first_line.max(1),
            })
            .collect();
        let active = self.active.and_then(|a| self.tabs.iter().position(|t| t.id == a));
        Session { version: crate::session::VERSION, active, tabs, recent: self.recent.clone() }
    }

    /// The Bus came up: subscribe topics, ping `edit`, reattach the session.
    pub fn start(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        for topic in [wire::TOPIC_CHANGED, "theme.changed", "noded.props.changed"] {
            fx.push(Effect::Subscribe { topic: topic.to_string() });
        }
        self.send_info(&mut fx);
        let out = Outgoing { verb: "edit.list".into(), body: "{}".into(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Recovered, &mut fx);
        if let Some(s) = self.restore.take() {
            let mut ids = Vec::new();
            for st in &s.tabs {
                let id = self.new_tab(st.path.clone());
                if let Some(x) = self.x.get_mut(&id) {
                    x.recovery_id = st.recovery_id.clone();
                }
                if let Some(t) = self.tab_mut(id) {
                    t.editor.sel = Selection { anchor: st.caret, head: st.caret };
                    t.editor.scroll.first_line = st.first_line.max(1);
                }
                ids.push(id);
                match (&st.path, &st.recovery_id) {
                    (Some(p), _) => self.send_open(id, Some(p.clone()), false, &mut fx),
                    (None, Some(_)) => self.send_list(id, &mut fx),
                    (None, None) => self.send_open(id, None, false, &mut fx),
                }
            }
            self.active = s.active.and_then(|i| ids.get(i).copied()).or(ids.first().copied());
        }
        fx
    }

    /// A transport delivery (reply, topic, timer, deadline, connection edge).
    pub fn on_incoming(&mut self, incoming: Incoming) -> Vec<Effect> {
        let mut fx = Vec::new();
        match incoming {
            Incoming::Reply { req, rc, body } => self.on_reply(req, rc, &body, &mut fx),
            Incoming::Deadline { req } => self.on_deadline(req, &mut fx),
            Incoming::Timer { id } => self.on_timer(id, &mut fx),
            Incoming::Topic { topic, body } => self.on_topic(&topic, &body, &mut fx),
            Incoming::Connection { up } => {
                if up {
                    // The reconnect edge: loss may have happened before any
                    // event reached us (plan §3.6, GLM F11).
                    self.last_event_seq = None;
                    for id in self.tab_ids() {
                        if let Some(step) = self.tab_mut(id).and_then(|t| t.mirror.as_mut()).map(Mirror::suspect) {
                            self.drive(id, step, &mut fx);
                        }
                    }
                    self.send_info(&mut fx);
                } else {
                    let all: Vec<u64> = self.waiters.iter().map(|w| w.id).collect();
                    for w in all {
                        self.finish_waiter(w, Err(("cancelled", "the Bus connection was lost".to_string())), &mut fx);
                    }
                }
            }
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// A `ced.*` / `app.*` Bus request.
    pub fn on_bus_command(&mut self, cmd: BusCommand) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.bus_command(&cmd, &mut fx);
        self.eval_waiters(&mut fx);
        fx
    }

    /// A menu / key / Bus action on `tab` (default: the active tab).
    pub fn on_action(&mut self, tab: Option<TabId>, action: ActionId, intent: Intent) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Err((_, msg)) = self.action_args(tab, action, None, intent, &mut fx) {
            fx.push(Effect::Notice { tab, notice: Notice::Message { level: Level::Warn, text: msg } });
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// A message from a tab's editor widget (window input: `Intent::ui`).
    pub fn on_editor(&mut self, tab: TabId, msg: EditorMsg) -> Vec<Effect> {
        let mut fx = Vec::new();
        match msg {
            EditorMsg::Command(c) => {
                self.stats.keys += 1;
                let started = Instant::now();
                let done = self.command(tab, c, Intent::ui(tab), &mut fx);
                self.record_model(started.elapsed().as_micros() as u64);
                if let Err(e) = done {
                    fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: e } });
                }
            }
            EditorMsg::ImeCommit(text) => {
                self.stats.keys += 1;
                if let Some(t) = self.tab_mut(tab) {
                    t.editor.set_preedit(false);
                }
                let started = Instant::now();
                let done = self.command(tab, EditCommand::Insert(text), Intent::ui(tab), &mut fx);
                self.record_model(started.elapsed().as_micros() as u64);
                if let Err(e) = done {
                    fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: e } });
                }
            }
            EditorMsg::Scrolled(s) => {
                if let Some(t) = self.tab_mut(tab) {
                    t.editor.scroll = s;
                }
                self.session_changed(&mut fx);
            }
            EditorMsg::Copy | EditorMsg::Cut => {
                if let Some(text) = self.selected_text(tab) {
                    fx.push(Effect::ClipboardWrite { text, primary: false });
                    if matches!(msg, EditorMsg::Cut)
                        && let Err(e) = self.command(tab, EditCommand::Delete, Intent::ui(tab), &mut fx)
                    {
                        fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: e } });
                    }
                }
            }
            EditorMsg::Paste { primary } => fx.push(Effect::ClipboardRead { primary, intent: Intent::ui(tab) }),
            EditorMsg::Preedit(s) => {
                if let Some(t) = self.tab_mut(tab) {
                    t.editor.set_preedit(!s.is_empty());
                }
            }
            EditorMsg::Focus(focused) => {
                if focused {
                    self.active = Some(tab);
                    if let Some(x) = self.x.get_mut(&tab) {
                        x.agent_since_focus = false;
                    }
                    self.timer(2_000, TimerFor::ClearMarkers(tab), &mut fx);
                    if self.tab(tab).is_some_and(|t| t.mirror.is_some()) {
                        self.send_saved(tab, &mut fx);
                    }
                }
            }
            EditorMsg::Layout(r) => {
                if let Some(x) = self.x.get_mut(&tab) {
                    x.layout = Some(r);
                }
            }
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// A clipboard read completed for `intent` (applies to `intent.tab`'s
    /// selection as it is now; dropped with a notice if that tab is gone).
    pub fn on_paste(&mut self, intent: Intent, text: Option<String>) -> Vec<Effect> {
        let mut fx = Vec::new();
        let tab = intent.tab;
        let live = self.tab(tab).and_then(|t| t.mirror.as_ref()).is_some_and(|m| matches!(m.phase(), Phase::Live));
        match text {
            Some(text) if live && !text.is_empty() => {
                if let Err(e) = self.command(tab, EditCommand::Insert(text), intent, &mut fx) {
                    fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: e } });
                }
            }
            Some(_) if !live => fx.push(Effect::Notice {
                tab: None,
                notice: Notice::Message { level: Level::Warn, text: "The paste was dropped: its tab is closed or detached".into() },
            }),
            _ => {}
        }
        self.eval_waiters(&mut fx);
        fx
    }

    /// Open paths (argv, `ced.open`, file drop, dialog). `path:line[:col]`
    /// suffixes are honoured when that file does not exist.
    pub fn open_paths(&mut self, paths: &[String], intent: Intent) -> Vec<Effect> {
        let _ = intent;
        let mut fx = Vec::new();
        self.open_many(paths, None, &mut fx);
        fx
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active(&self) -> Option<TabId> {
        self.active
    }

    /// Another origin edited this tab since it was last focused (tab `◆`).
    pub fn agent_since_focus(&self, tab: TabId) -> bool {
        self.x.get(&tab).is_some_and(|x| x.agent_since_focus)
    }

    // ── tabs ─────────────────────────────────────────────────────────────────

    fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    fn tab(&self, id: TabId) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn new_tab(&mut self, path: Option<String>) -> TabId {
        let id = self.next_tab;
        self.next_tab += 1;
        let highlight = Highlight::for_language("text", path.as_deref().map(std::path::Path::new));
        self.tabs.push(Tab { id, mirror: None, editor: EditorModel::default(), highlight, diagnostics: Diagnostics::default(), path });
        self.x.insert(id, TabX::default());
        id
    }

    /// The tab a Bus request names (`tab` or `buffer`), else the active one.
    fn resolve_tab(&self, sel: &verbs::TabSel) -> Result<TabId, String> {
        if let Some(t) = sel.tab {
            return self.tab(t).map(|t| t.id).ok_or_else(|| format!("no tab {t}"));
        }
        if let Some(b) = &sel.buffer {
            return self
                .tabs
                .iter()
                .find(|t| t.mirror.as_ref().is_some_and(|m| m.buffer() == b))
                .map(|t| t.id)
                .ok_or_else(|| format!("no tab shows buffer {b}"));
        }
        self.active.ok_or_else(|| "no tab is open".to_string())
    }

    fn close_tab(&mut self, id: TabId, fx: &mut Vec<Effect>) {
        self.tabs.retain(|t| t.id != id);
        self.x.remove(&id);
        let waiters: Vec<u64> = self.waiters.iter().filter(|w| w.tab == id).map(|w| w.id).collect();
        for w in waiters {
            self.finish_waiter(w, Err(("cancelled", "the tab was closed".to_string())), fx);
        }
        if self.active == Some(id) {
            self.active = self.tabs.last().map(|t| t.id);
        }
        self.session_changed(fx);
    }

    // ── requests ─────────────────────────────────────────────────────────────

    fn send(&mut self, out: Outgoing, req: Req, fx: &mut Vec<Effect>) {
        self.next_req += 1;
        self.reqs.insert(self.next_req, req);
        fx.push(Effect::Send { req: self.next_req, out });
    }

    fn timer(&mut self, ms: u64, what: TimerFor, fx: &mut Vec<Effect>) -> u64 {
        self.next_timer += 1;
        self.timers.insert(self.next_timer, what);
        fx.push(Effect::Timer { id: self.next_timer, ms });
        self.next_timer
    }

    fn send_info(&mut self, fx: &mut Vec<Effect>) {
        let out = Outgoing { verb: "edit.info".into(), body: "{}".into(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Info, fx);
    }

    fn send_open(&mut self, tab: TabId, path: Option<String>, reattach: bool, fx: &mut Vec<Effect>) {
        let body = match &path {
            Some(p) => json!({"path": p, "origin": UI_ORIGIN}),
            None => json!({"origin": UI_ORIGIN}),
        };
        let out = Outgoing { verb: "edit.open".into(), body: body.to_string(), op_id: None, deadline_ms: DEADLINE_LONG_MS };
        self.send(out, Req::Open { tab, reattach }, fx);
    }

    fn send_list(&mut self, tab: TabId, fx: &mut Vec<Effect>) {
        let out = Outgoing { verb: "edit.list".into(), body: "{}".into(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::List { tab }, fx);
    }

    fn on_reply(&mut self, req: u64, rc: u8, body: &str, fx: &mut Vec<Effect>) {
        match self.reqs.remove(&req) {
            Some(Req::Mirror { tab, op_id }) => {
                let result = reply_result(rc, body);
                if let Ok(v) = &result
                    && let Some(ep) = v.get("epoch").and_then(Value::as_str)
                {
                    self.note_epoch(ep, fx);
                }
                if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(|m| m.on_reply(&op_id, result)) {
                    self.drive(tab, step, fx);
                }
            }
            Some(Req::Open { tab, reattach }) => self.on_open_reply(tab, reattach, rc, body, fx),
            Some(Req::List { tab }) => self.on_list_reply(tab, rc, body, fx),
            Some(Req::Info) => {
                if rc < 10
                    && let Ok(info) = serde_json::from_str::<wire::InfoReply>(body)
                {
                    self.edit_info = verbs::EditInfo {
                        epoch: Some(info.epoch.clone()),
                        version: Some(info.version.clone()),
                        volatile: Some(info.volatile),
                    };
                    self.note_epoch(&info.epoch, fx);
                }
            }
            Some(Req::Close { tab, cmd, intent }) => {
                if rc < 10 {
                    self.close_tab(tab, fx);
                    if let Some((id, action)) = cmd {
                        fx.push(Effect::Respond { id, rc: 0, body: ok_body(&verbs::ActionReply { id: action, ok: true, result: None }) });
                    }
                } else {
                    let r = reply_result(rc, body).err();
                    let message = r.as_ref().map(|r| r.message.clone()).unwrap_or_else(|| body.to_string());
                    let dirty = r.as_ref().and_then(|r| r.reason.as_deref()) == Some("dirty");
                    match cmd {
                        Some((id, _)) => fx.push(Effect::Respond {
                            id,
                            rc: 10,
                            body: refusal(code::CONFLICT, message, Some(if dirty { "dirty" } else { "refused" })),
                        }),
                        None if dirty => fx.push(Effect::Prompt(Prompt::CloseDirty { tab, intent })),
                        None => fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: message } }),
                    }
                }
            }
            Some(Req::Find { tab, job }) => self.on_find_reply(tab, *job, rc, body, fx),
            Some(Req::Matches { tab, query }) => self.on_matches_reply(tab, query, rc, body, fx),
            Some(Req::Recovered) => self.on_recovered_list(rc, body, fx),
            Some(Req::Saved { tab }) => self.on_saved_reply(tab, rc, body),
            Some(Req::Discard) => {
                if rc >= 10 {
                    let msg = reply_result(rc, body).err().map(|r| r.message).unwrap_or_default();
                    fx.push(Effect::Notice { tab: None, notice: Notice::Message { level: Level::Warn, text: msg } });
                }
            }
            Some(Req::Select) | None => {}
        }
    }

    fn on_deadline(&mut self, req: u64, fx: &mut Vec<Effect>) {
        match self.reqs.remove(&req) {
            Some(Req::Mirror { tab, op_id }) => {
                self.stats.uncertain += 1;
                if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(|m| m.on_deadline(&op_id)) {
                    self.drive(tab, step, fx);
                }
            }
            Some(Req::Open { tab, reattach }) => {
                // Opening is idempotent for a path; ask again.
                let path = self.tab(tab).and_then(|t| t.path.clone());
                self.send_open(tab, path, reattach, fx);
                let misses = self.x.get_mut(&tab).map(|x| {
                    x.open_misses += 1;
                    x.open_misses
                });
                if misses == Some(OPEN_WARN_AFTER) {
                    let text = cosmix_edit_client::mirror::MSG_SERVICE_SILENT.to_string();
                    fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text } });
                }
            }
            Some(Req::List { tab }) => self.send_list(tab, fx),
            Some(Req::Find { tab, .. }) => {
                fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Warn, text: "The search timed out".into() } })
            }
            Some(Req::Close { cmd: Some((id, _)), .. }) => {
                fx.push(Effect::Respond { id, rc: 10, body: refusal(code::UNAVAILABLE, "edit.close timed out", Some("timeout")) });
            }
            _ => {}
        }
    }

    fn on_timer(&mut self, id: u64, fx: &mut Vec<Effect>) {
        match self.timers.remove(&id) {
            Some(TimerFor::Retry(tab)) => {
                if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(Mirror::on_retry) {
                    self.drive(tab, step, fx);
                }
            }
            Some(TimerFor::Wait(w)) => {
                let ms = self.waiters.iter().find(|x| x.id == w).map(|x| x.timeout_ms).unwrap_or(0);
                self.finish_waiter(w, Err(("timeout", format!("ced.wait timed out after {ms} ms"))), fx);
            }
            Some(TimerFor::Publish(tab)) => {
                if let Some(x) = self.x.get_mut(&tab) {
                    x.publish_timer = false;
                }
                self.maybe_publish(tab, fx);
            }
            Some(TimerFor::Session) => {
                self.session_timer = false;
                fx.push(Effect::SaveSession);
            }
            Some(TimerFor::Relex(tab)) => self.relex_due(tab, fx),
            Some(TimerFor::Rematch(tab)) => {
                if let Some(x) = self.x.get_mut(&tab) {
                    x.rematch_timer = false;
                }
                self.request_matches(tab, fx);
            }
            Some(TimerFor::UiDeadline(token)) => self.ui_timeout(token, fx),
            Some(TimerFor::Echo(tab, op_id)) => {
                if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(|m| m.on_echo_timeout(&op_id)) {
                    if !step.out.is_empty() {
                        self.stats.history_recoveries += 1;
                    }
                    self.drive(tab, step, fx);
                }
            }
            Some(TimerFor::OpWait(op_id)) => {
                if let Some((id, action)) = self.op_waits.remove(&op_id) {
                    let msg = format!("{action} is still queued or in flight after {} s; it may yet complete", OP_WAIT_MS / 1000);
                    fx.push(Effect::Respond { id, rc: 10, body: refusal("TIMEOUT", msg, Some("timeout")) });
                }
            }
            Some(TimerFor::ClearMarkers(tab)) => {
                if self.active == Some(tab)
                    && let Some(t) = self.tab_mut(tab)
                {
                    t.editor.clear_markers();
                }
            }
            None => {}
        }
    }

    fn on_topic(&mut self, topic: &str, body: &str, fx: &mut Vec<Effect>) {
        match topic {
            wire::TOPIC_CHANGED => {
                let Ok(ev) = serde_json::from_str::<wire::Event>(body) else { return };
                self.on_edit_event(ev, fx);
            }
            // `edit` (re)appearing in services.registered may be a restart:
            // ask it its epoch (plan §3.6).
            "noded.props.changed" if body.contains("services.registered") && body.contains("\"edit\"") => {
                self.send_info(fx);
            }
            _ => {}
        }
    }

    fn on_edit_event(&mut self, ev: wire::Event, fx: &mut Vec<Effect>) {
        self.stats.events += 1;
        let (epoch, seq) = match &ev {
            wire::Event::Edit(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Cursor(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Anchor(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Disk(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Open(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Close(e) => (e.epoch.clone(), e.event_seq),
            wire::Event::Resync(e) => (e.epoch.clone(), e.event_seq),
        };
        if self.note_epoch(&epoch, fx) {
            self.last_event_seq = Some(seq);
            return;
        }
        // The global event_seq: any gap may have been ours (plan §3.6).
        if let Some(last) = self.last_event_seq
            && seq != last + 1
        {
            for id in self.tab_ids() {
                if let Some(step) = self.tab_mut(id).and_then(|t| t.mirror.as_mut()).map(Mirror::suspect) {
                    self.stats.history_recoveries += 1;
                    self.drive(id, step, fx);
                }
            }
        }
        self.last_event_seq = Some(seq);
        let buffer = match &ev {
            wire::Event::Edit(e) => Some(e.buffer.clone()),
            wire::Event::Cursor(e) => Some(e.buffer.clone()),
            wire::Event::Disk(e) => Some(e.buffer.clone()),
            wire::Event::Close(e) => Some(e.buffer.clone()),
            wire::Event::Resync(_) => None,
            _ => return,
        };
        for id in self.tab_ids() {
            let step = match self.tab_mut(id).and_then(|t| t.mirror.as_mut()) {
                Some(m) if buffer.as_deref().is_none_or(|b| b == m.buffer()) => m.on_event(&ev),
                _ => continue,
            };
            if let wire::Event::Edit(e) = &ev
                && !(e.origin == UI_ORIGIN || e.origin.starts_with("agent:ced."))
                && self.active != Some(id)
                && let Some(x) = self.x.get_mut(&id)
            {
                x.agent_since_focus = true;
            }
            self.drive(id, step, fx);
            if matches!(ev, wire::Event::Disk(_)) {
                // A save (by anyone) or reload changed the disk state.
                self.send_saved(id, fx);
            }
        }
    }

    /// Record the daemon's epoch; `true` when it CHANGED (every tab reattaches).
    fn note_epoch(&mut self, epoch: &str, fx: &mut Vec<Effect>) -> bool {
        match &self.edit_epoch {
            Some(e) if e == epoch => false,
            None => {
                self.edit_epoch = Some(epoch.to_string());
                false
            }
            Some(_) => {
                self.edit_epoch = Some(epoch.to_string());
                self.edit_info.epoch = Some(epoch.to_string());
                self.last_event_seq = None;
                for id in self.tab_ids() {
                    let stale = self.tab(id).and_then(|t| t.mirror.as_ref()).is_some_and(|m| m.epoch() != epoch);
                    if stale {
                        self.reattach(id, fx);
                    }
                }
                true
            }
        }
    }

    /// Plan §3.8: reopen the tab's buffer in the new daemon session.
    fn reattach(&mut self, tab: TabId, fx: &mut Vec<Effect>) {
        let Some(x) = self.x.get_mut(&tab) else { return };
        if x.reattaching {
            return;
        }
        x.reattaching = true;
        if let Some(step) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()).map(Mirror::epoch_changed) {
            self.drive(tab, step, fx);
        }
        let path = self.tab(tab).and_then(|t| t.path.clone());
        match path {
            Some(p) => self.send_open(tab, Some(p), true, fx),
            None => self.send_list(tab, fx),
        }
    }

    fn on_open_reply(&mut self, tab: TabId, reattach: bool, rc: u8, body: &str, fx: &mut Vec<Effect>) {
        if self.tab(tab).is_none() {
            return;
        }
        match reply_result(rc, body) {
            Ok(v) => match serde_json::from_value::<wire::OpenReply>(v) {
                Ok(open) => self.attach(tab, reattach, &open, fx),
                Err(e) => self.open_failed(tab, format!("bad edit.open reply: {e}"), fx),
            },
            Err(r) => {
                let msg = match r.reason.as_deref() {
                    Some(reason) => format!("{} ({reason})", r.message),
                    None => r.message.clone(),
                };
                self.open_failed(tab, msg, fx);
            }
        }
    }

    /// A scratch tab reattaches by `recovery_id` (plan §3.8); with no match it
    /// becomes a fresh scratch buffer and keeps its text as a detached copy.
    fn on_list_reply(&mut self, tab: TabId, rc: u8, body: &str, fx: &mut Vec<Effect>) {
        let rid = self.x.get(&tab).and_then(|x| x.recovery_id.clone());
        let list = reply_result(rc, body).ok().and_then(|v| serde_json::from_value::<wire::ListReply>(v).ok());
        let row = list.as_ref().and_then(|l| l.buffers.iter().find(|b| Some(&b.recovery_id) == rid.as_ref()).map(|b| (l, b)));
        match row {
            Some((l, b)) => {
                let open = open_reply_of(&l.epoch, b);
                let reattach = self.x.get(&tab).is_some_and(|x| x.reattaching);
                self.attach(tab, reattach, &open, fx);
            }
            None => {
                let reattach = self.x.get(&tab).is_some_and(|x| x.reattaching);
                self.send_open(tab, None, reattach, fx);
            }
        }
    }

    fn attach(&mut self, tab: TabId, reattach: bool, open: &wire::OpenReply, fx: &mut Vec<Effect>) {
        self.note_epoch(&open.epoch, fx);
        let reattaching = reattach && self.tab(tab).is_some_and(|t| t.mirror.is_some());
        let step = if reattaching {
            let Some(m) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()) else { return };
            m.reattach(open)
        } else {
            match Mirror::bootstrap(open) {
                Ok((m, step)) => {
                    let path = open.path.clone();
                    if let Some(t) = self.tab_mut(tab) {
                        t.highlight = Highlight::for_language(&open.language, path.as_deref().map(std::path::Path::new));
                        t.mirror = Some(m);
                        if t.path.is_none() {
                            t.path = path;
                        }
                    }
                    step
                }
                Err(e) => {
                    self.open_failed(tab, format!("{e:?}"), fx);
                    return;
                }
            }
        };
        if let Some(x) = self.x.get_mut(&tab) {
            x.recovery_id = Some(open.recovery_id.clone());
            x.opened = true;
            x.open_misses = 0;
            x.reattaching = false;
            x.open_error = None;
        }
        if let Some(p) = &open.path {
            self.recent.retain(|r| r != p);
            self.recent.insert(0, p.clone());
            self.recent.truncate(RECENT_MAX);
        }
        self.drive(tab, step, fx);
        self.send_saved(tab, fx);
        self.check_opens(fx);
        self.session_changed(fx);
    }

    fn open_failed(&mut self, tab: TabId, msg: String, fx: &mut Vec<Effect>) {
        if let Some(x) = self.x.get_mut(&tab) {
            x.opened = true;
            x.reattaching = false;
            x.open_error = Some(msg.clone());
        }
        fx.push(Effect::Notice { tab: Some(tab), notice: Notice::Message { level: Level::Error, text: msg } });
        self.check_opens(fx);
    }

    /// Answer every `ced.open` / `ced.new` whose tabs have all had an answer.
    fn check_opens(&mut self, fx: &mut Vec<Effect>) {
        let mut i = 0;
        while i < self.opens.len() {
            let done = self.opens[i].tabs.iter().all(|t| self.x.get(t).is_none_or(|x| x.opened));
            if !done {
                i += 1;
                continue;
            }
            let p = self.opens.remove(i);
            let row = |c: &Controller, t: TabId| {
                let tab = c.tab(t);
                verbs::OpenedTab {
                    tab: t,
                    buffer: tab.and_then(|t| t.mirror.as_ref()).map(|m| m.buffer().to_string()),
                    path: tab.and_then(|t| t.path.clone()),
                }
            };
            let body = if p.new {
                let r = row(self, p.tabs[0]);
                ok_body(&verbs::NewReply { tab: r.tab, buffer: r.buffer })
            } else {
                let tabs = p.tabs.iter().map(|&t| row(self, t)).collect();
                ok_body(&verbs::OpenReply { tabs })
            };
            fx.push(Effect::Respond { id: p.cmd, rc: 0, body });
        }
    }

    /// Open (or focus) each path; `cmd` answers when all have an answer.
    fn open_many(&mut self, paths: &[String], cmd: Option<(u64, Option<(usize, usize)>)>, fx: &mut Vec<Effect>) -> Vec<TabId> {
        let mut ids = Vec::new();
        for raw in paths {
            let (path, goto) = if std::path::Path::new(raw).exists() { (raw.clone(), None) } else { split_line_col(raw) };
            let goto = cmd.and_then(|(_, g)| g).or(goto);
            let path = std::path::absolute(&path).map(|p| p.to_string_lossy().into_owned()).unwrap_or(path);
            let existing = self.tabs.iter().find(|t| t.path.as_deref() == Some(path.as_str())).map(|t| t.id);
            let id = match existing {
                Some(id) => id,
                None => {
                    let id = self.new_tab(Some(path.clone()));
                    self.send_open(id, Some(path), false, fx);
                    id
                }
            };
            if let Some(g) = goto {
                self.goto(id, g);
            }
            self.active = Some(id);
            ids.push(id);
        }
        if let Some((cmd, _)) = cmd {
            self.opens.push(PendingOpen { cmd, tabs: ids.clone(), new: false });
            self.check_opens(fx);
        }
        self.session_changed(fx);
        ids
    }

    /// Move the caret to `line:col` now if the text is there, else once it is.
    fn goto(&mut self, tab: TabId, (line, col): (usize, usize)) {
        let Some(t) = self.tab_mut(tab) else { return };
        let at = t.mirror.as_ref().filter(|m| matches!(m.phase(), Phase::Live)).map(|m| offset_of_line_col(m.text(), line, col));
        match at {
            Some(Ok(o)) => {
                t.editor.sel = Selection { anchor: o, head: o };
                t.editor.scroll.first_line = line.saturating_sub(5).max(1);
            }
            Some(Err(_)) => {}
            None => {
                if let Some(x) = self.x.get_mut(&tab) {
                    x.goto = Some((line, col));
                }
            }
        }
    }

    // ── the mirror ↔ editor bridge ───────────────────────────────────────────

    /// Feed a mirror step to the editor side and the transport; drain the
    /// scheduler; react to detaches, drains and pending gotos.
    fn drive(&mut self, tab: TabId, step: Step, fx: &mut Vec<Effect>) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) else { return };
        let mut resynced = false;
        if let Some(m) = &t.mirror {
            for d in &step.deltas {
                t.editor.apply_delta(d);
                t.highlight.apply_delta(m.text(), d);
                t.diagnostics.apply_delta(d);
                if d.kind == DeltaKind::Resync {
                    t.editor.clamp(m.text());
                    resynced = true;
                }
            }
        }
        let is_mix = t.highlight.is_mix();
        if !step.deltas.is_empty() {
            self.after_deltas(tab, &step.deltas, is_mix, fx);
        }
        if let Some(c) = self.x.get_mut(&tab).and_then(|x| x.lint.as_mut()) {
            for d in &step.deltas {
                c.record(d);
            }
        }
        if resynced {
            self.stats.snapshot_recoveries += 1;
        }
        for n in step.notices {
            if matches!(n, Notice::Conflict(_)) {
                self.stats.conflicts += 1;
            }
            if matches!(&n, Notice::Message { text, .. } if text == cosmix_edit_client::mirror::MSG_SAVE_DISK_MODIFIED) {
                let x = self.x.get_mut(&tab);
                let intent = x.and_then(|x| {
                    x.close_after_save = None;
                    x.save_intent.take()
                });
                let intent = intent.unwrap_or_else(|| Intent::ui(tab));
                // Only the human who pressed Save is asked; a Bus caller gets
                // the refusal as its `ced.action` reply (Opus m4).
                if matches!(intent.by, Invoker::Ui) {
                    fx.push(Effect::Prompt(Prompt::DiskModified { tab, intent }));
                    continue;
                }
            }
            fx.push(Effect::Notice { tab: Some(tab), notice: n });
        }
        let (outcomes, echo) = match self.tab_mut(tab).and_then(|t| t.mirror.as_mut()) {
            Some(m) => (m.take_outcomes(), m.take_echo_timer()),
            None => (Vec::new(), None),
        };
        for (op_id, outcome) in outcomes {
            self.answer_op(&op_id, outcome, fx);
        }
        if let Some((op_id, ms)) = echo {
            self.timer(ms, TimerFor::Echo(tab, op_id), fx);
        }
        for out in step.out {
            let op_id = out.op_id.clone().unwrap_or_default();
            self.send(out, Req::Mirror { tab, op_id }, fx);
        }
        let mut outs = Vec::new();
        let mut retry = None;
        let mut detached_epoch = false;
        let mut idle = false;
        let mut live = false;
        if let Some(m) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()) {
            while let Some(o) = m.next_outgoing() {
                outs.push(o);
            }
            retry = m.take_retry_timer();
            detached_epoch = matches!(m.phase(), Phase::Detached { reason: DetachReason::EpochChanged });
            idle = m.is_idle();
            live = matches!(m.phase(), Phase::Live);
        }
        for out in outs {
            let op_id = out.op_id.clone().unwrap_or_default();
            self.send(out, Req::Mirror { tab, op_id }, fx);
        }
        if let Some(ms) = retry {
            self.stats.retries += 1;
            self.timer(ms, TimerFor::Retry(tab), fx);
        }
        if detached_epoch && !self.x.get(&tab).is_some_and(|x| x.reattaching) {
            // `epoch_mismatch` seen by the mirror itself: learn the new epoch.
            self.send_info(fx);
            self.reattach(tab, fx);
        }
        if live && let Some(g) = self.x.get_mut(&tab).and_then(|x| x.goto.take()) {
            self.goto(tab, g);
        }
        if live && let Some((text, intent)) = self.x.get_mut(&tab).and_then(|x| x.fill.take()) {
            let len = self.tab(tab).and_then(|t| t.mirror.as_ref()).map_or(0, |m| m.text().len());
            let e = cosmix_edit_client::types::LocalEdit {
                items: vec![(len..len, text)],
                coalesce: false,
                caret_after: Selection { anchor: 0, head: 0 },
            };
            if let Some(i) = self.tabs.iter().position(|t| t.id == tab)
                && let Some(m) = self.tabs[i].mirror.as_mut()
            {
                match m.local_edit(e, intent, &mut self.ids) {
                    Ok(step) => return self.drive(tab, step, fx),
                    Err(e) => fx.push(Effect::Notice {
                        tab: Some(tab),
                        notice: Notice::Message { level: Level::Error, text: format!("the copy could not be inserted: {e:?}") },
                    }),
                }
            }
        }
        if idle {
            if let Some(job) = self.x.get_mut(&tab).and_then(|x| x.find.take()) {
                self.start_find(tab, job, fx);
            }
            if self.x.get_mut(&tab).is_some_and(|x| std::mem::take(&mut x.rematch_due)) {
                self.request_matches(tab, fx);
            }
            let saved = self.tab(tab).and_then(|t| t.mirror.as_ref()).is_some_and(|m| !m.meta().dirty);
            if let Some(intent) = self.x.get_mut(&tab).and_then(|x| x.close_after_save.take_if(|_| saved)) {
                self.close(tab, false, None, intent, fx);
                return;
            }
            self.maybe_publish(tab, fx);
        }
    }

    fn edit_cfg(&self, tab: &Tab) -> EditCfg {
        let (language, eol) = tab.mirror.as_ref().map(|m| (m.meta().language.clone(), m.meta().eol)).unwrap_or(("text".into(), wire::Eol::Lf));
        let insert_spaces = self.config.insert_spaces.get(&language).copied().unwrap_or(false);
        // With spaces, one indent = `tab_size` spaces: pass the language's
        // indent width in the editing cfg (E1e contract).
        let tab_size = if insert_spaces { self.config.indent_width(&language) } else { self.config.tab_size };
        EditCfg {
            measure: MeasureCfg { tab_size: tab_size.clamp(1, 16), ambiguous_wide: self.config.ambiguous_wide },
            insert_spaces,
            eol: if eol == wire::Eol::Crlf { "\r\n" } else { "\n" },
            line_comment: cosmix_edit_client::model::line_comment_for(&language),
        }
    }

    /// Run an editing / motion command on `tab` for `intent`.
    fn command(&mut self, tab: TabId, c: EditCommand, intent: Intent, fx: &mut Vec<Effect>) -> Result<(), String> {
        let Some(i) = self.tabs.iter().position(|t| t.id == tab) else { return Err(format!("no tab {tab}")) };
        let cfg = self.edit_cfg(&self.tabs[i]);
        let t = &mut self.tabs[i];
        let Some(m) = t.mirror.as_mut() else { return Err("the buffer is not open yet".into()) };
        let before = t.editor.sel;
        let Some(edit) = t.editor.command(m.text(), &cfg, c) else {
            if t.editor.sel != before {
                self.caret_moved(tab, fx);
            }
            return Ok(());
        };
        let caret_after = edit.caret_after;
        let step = m.local_edit(edit, intent, &mut self.ids).map_err(|e| match e {
            cosmix_edit_client::mirror::MirrorError::NotLive => "The buffer is reconnecting — try again in a moment".to_string(),
            cosmix_edit_client::mirror::MirrorError::TooLarge { items, bytes } => {
                format!("Too large for one undoable edit ({items} places, {bytes} bytes)")
            }
            cosmix_edit_client::mirror::MirrorError::Invalid(m) => m,
        })?;
        self.drive(tab, step, fx);
        // The issuing view's selection is the edit's `caret_after`: mapping
        // alone cannot express a moved or re-indented block (E1e contract).
        if let Some(t) = self.tab_mut(tab) {
            t.editor.sel = caret_after;
        }
        self.caret_moved(tab, fx);
        Ok(())
    }

    fn selected_text(&self, tab: TabId) -> Option<String> {
        let t = self.tab(tab)?;
        let m = t.mirror.as_ref()?;
        let s = t.editor.sel;
        let r: Range<usize> = s.anchor.min(s.head)..s.anchor.max(s.head);
        if r.is_empty() || r.end > m.text().len() {
            return None;
        }
        let mut out = String::new();
        m.text().read(r, &mut out);
        Some(out)
    }

    fn server_op(&mut self, tab: TabId, op: ServerOp, intent: Intent, fx: &mut Vec<Effect>) -> Result<(), String> {
        let Some(m) = self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| t.mirror.as_mut()) else {
            return Err("the buffer is not open".into());
        };
        if matches!(m.phase(), Phase::Detached { .. }) {
            return Err("the buffer is detached".into());
        }
        // The id `server_op` is about to hand out: a Bus `ced.action` being
        // dispatched is answered from this op's outcome, not now.
        let op_id = self.ids.clone().next_id();
        let step = m.server_op(op, intent, &mut self.ids);
        if let Some(cmd) = self.op_cmd.take() {
            self.op_waits.insert(op_id.clone(), cmd);
            self.timer(OP_WAIT_MS, TimerFor::OpWait(op_id), fx);
        }
        self.drive(tab, step, fx);
        Ok(())
    }

    /// Answer the Bus `ced.action` waiting on server op `op_id`, if any.
    fn answer_op(&mut self, op_id: &str, outcome: Outcome, fx: &mut Vec<Effect>) {
        let Some((id, action)) = self.op_waits.remove(op_id) else { return };
        self.timers.retain(|_, t| !matches!(t, TimerFor::OpWait(x) if x == op_id));
        let (rc, body) = match outcome {
            Outcome::Done => (0, ok_body(&verbs::ActionReply { id: action, ok: true, result: None })),
            Outcome::Refused(r) => {
                let code = serde_json::to_value(r.error_code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                (10, refusal(&code, r.message, r.reason.as_deref()))
            }
        };
        fx.push(Effect::Respond { id, rc, body });
    }

    /// Ask the service for the tab's save state (Opus m2): after an attach,
    /// on a disk-state change and when the tab gains focus — the open reply
    /// carries none, and another holder may save behind ced's back.
    fn send_saved(&mut self, tab: TabId, fx: &mut Vec<Effect>) {
        let out = Outgoing { verb: "edit.list".into(), body: "{}".into(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Saved { tab }, fx);
    }

    fn on_saved_reply(&mut self, tab: TabId, rc: u8, body: &str) {
        let Some(list) = reply_result(rc, body).ok().and_then(|v| serde_json::from_value::<wire::ListReply>(v).ok()) else { return };
        let Some(m) = self.tab_mut(tab).and_then(|t| t.mirror.as_mut()) else { return };
        if list.epoch != m.epoch() {
            return;
        }
        if let Some(row) = list.buffers.iter().find(|b| b.buffer == m.buffer()) {
            m.note_saved(row.saved_rev, row.recovered);
        }
    }

    // ── selection publishing (plan §3.3) ─────────────────────────────────────

    fn caret_moved(&mut self, tab: TabId, fx: &mut Vec<Effect>) {
        let arm = self.x.get_mut(&tab).is_some_and(|x| {
            let arm = !x.publish_timer;
            x.publish_timer = true;
            arm
        });
        if arm {
            self.timer(PUBLISH_DEBOUNCE_MS, TimerFor::Publish(tab), fx);
        }
        self.session_changed(fx);
    }

    fn maybe_publish(&mut self, tab: TabId, fx: &mut Vec<Effect>) {
        let Some(t) = self.tab(tab) else { return };
        let Some(m) = t.mirror.as_ref().filter(|m| m.is_idle()) else { return };
        let sel = t.editor.sel;
        let buffer = m.buffer().to_string();
        let len = m.text().len();
        let Some(x) = self.x.get_mut(&tab) else { return };
        if x.published == Some(sel) || x.publish_timer || sel.anchor.max(sel.head) > len {
            return;
        }
        if let Some(last) = x.last_publish {
            let gap = last.elapsed().as_millis() as u64;
            if gap < PUBLISH_MIN_GAP_MS {
                x.publish_timer = true;
                self.timer(PUBLISH_MIN_GAP_MS - gap, TimerFor::Publish(tab), fx);
                return;
            }
        }
        x.published = Some(sel);
        x.last_publish = Some(Instant::now());
        let body = json!({"buffer": buffer, "ranges": [{"anchor": sel.anchor, "head": sel.head}], "origin": UI_ORIGIN});
        let out = Outgoing { verb: "edit.select".into(), body: body.to_string(), op_id: None, deadline_ms: DEADLINE_MS };
        self.send(out, Req::Select, fx);
    }

    fn session_changed(&mut self, fx: &mut Vec<Effect>) {
        if !self.session_timer {
            self.session_timer = true;
            self.timer(SESSION_DEBOUNCE_MS, TimerFor::Session, fx);
        }
    }

    // ── actions ──────────────────────────────────────────────────────────────

    /// `Ok(result)` when handled here; `Err((code, message))` otherwise.
    fn action(&mut self, tab: Option<TabId>, action: ActionId, intent: Intent, fx: &mut Vec<Effect>) -> Result<Option<Value>, (&'static str, String)> {
        let need = |t: Option<TabId>| t.ok_or((code::NOT_FOUND, "no tab is open".to_string()));
        let tab = tab.or(self.active);
        let edit = |c: &mut Controller, t: TabId, cmd: EditCommand, intent: Intent, fx: &mut Vec<Effect>| {
            c.command(t, cmd, intent, fx).map(|()| None).map_err(|e| (code::CONFLICT, e))
        };
        let conflict = |e: String| (code::CONFLICT, e);
        match action {
            ActionId::FileNew => {
                let id = self.new_tab(None);
                self.send_open(id, None, false, fx);
                self.active = Some(id);
                Ok(Some(json!({"tab": id})))
            }
            ActionId::FileSave => {
                let t = need(tab)?;
                self.server_op(t, ServerOp::Save { path: None, force: false }, intent, fx).map_err(conflict)?;
                Ok(None)
            }
            ActionId::FileSaveAll => {
                for t in self.tab_ids() {
                    let dirty = self.tab(t).and_then(|t| t.mirror.as_ref()).is_some_and(|m| m.meta().dirty && m.meta().path.is_some());
                    if dirty {
                        let _ = self.server_op(t, ServerOp::Save { path: None, force: false }, intent.clone(), fx);
                    }
                }
                Ok(None)
            }
            ActionId::FileReload => {
                let t = need(tab)?;
                self.server_op(t, ServerOp::Reload { force: false }, intent, fx).map_err(conflict)?;
                Ok(None)
            }
            ActionId::FileClose => {
                let t = need(tab)?;
                self.close(t, false, None, intent, fx);
                Ok(None)
            }
            ActionId::FileExit => {
                fx.push(Effect::SaveSession);
                fx.push(Effect::Quit);
                Ok(None)
            }
            ActionId::EditUndo | ActionId::EditRedo | ActionId::EditUndoAny | ActionId::EditUndoOther => {
                let t = need(tab)?;
                let lane = match action {
                    ActionId::EditUndoAny => LaneArg::Any,
                    ActionId::EditUndoOther => {
                        let lane = self
                            .tab(t)
                            .and_then(|t| t.mirror.as_ref())
                            .and_then(|m| m.last_remote().map(|r| r.lane.clone()))
                            .ok_or((code::CONFLICT, "no other origin has edited this buffer".to_string()))?;
                        LaneArg::Lane(lane)
                    }
                    _ => LaneArg::Own,
                };
                let op = if action == ActionId::EditRedo { ServerOp::Redo { lane } } else { ServerOp::Undo { lane } };
                self.server_op(t, op, intent, fx).map_err(conflict)?;
                Ok(None)
            }
            ActionId::EditCopy | ActionId::EditCut => {
                let t = need(tab)?;
                let text = self.selected_text(t);
                if let Some(text) = &text {
                    fx.push(Effect::ClipboardWrite { text: text.clone(), primary: false });
                    if action == ActionId::EditCut {
                        edit(self, t, EditCommand::Delete, intent, fx)?;
                    }
                }
                Ok(text.map(Value::String))
            }
            ActionId::EditPaste => {
                let t = need(tab)?;
                let intent = Intent { tab: t, ..intent };
                fx.push(Effect::ClipboardRead { primary: false, intent });
                Ok(None)
            }
            ActionId::EditSelectAll => edit(self, need(tab)?, EditCommand::SelectAll, intent, fx),
            ActionId::EditDuplicateLine => edit(self, need(tab)?, EditCommand::DuplicateLine, intent, fx),
            ActionId::EditDeleteLine => edit(self, need(tab)?, EditCommand::DeleteLine, intent, fx),
            ActionId::EditMoveLineUp => edit(self, need(tab)?, EditCommand::MoveLineUp, intent, fx),
            ActionId::EditMoveLineDown => edit(self, need(tab)?, EditCommand::MoveLineDown, intent, fx),
            ActionId::EditToggleComment => edit(self, need(tab)?, EditCommand::ToggleComment, intent, fx),
            ActionId::EditIndent => edit(self, need(tab)?, EditCommand::Tab, intent, fx),
            ActionId::EditOutdent => edit(self, need(tab)?, EditCommand::Outdent, intent, fx),
            ActionId::EditDeleteWordLeft => edit(self, need(tab)?, EditCommand::DeleteWordLeft, intent, fx),
            ActionId::EditDeleteWordRight => edit(self, need(tab)?, EditCommand::DeleteWordRight, intent, fx),
            ActionId::EditOverwrite => {
                let t = need(tab)?;
                let on = self.tab_mut(t).map(|t| {
                    t.editor.overwrite = !t.editor.overwrite;
                    t.editor.overwrite
                });
                Ok(on.map(Value::Bool))
            }
            ActionId::TabsNext | ActionId::TabsPrev => {
                let n = self.tabs.len();
                if n == 0 {
                    return Err((code::NOT_FOUND, "no tab is open".into()));
                }
                let i = self.active.and_then(|a| self.tabs.iter().position(|t| t.id == a)).unwrap_or(0);
                let j = if action == ActionId::TabsNext { (i + 1) % n } else { (i + n - 1) % n };
                self.active = Some(self.tabs[j].id);
                self.session_changed(fx);
                Ok(Some(json!({"tab": self.tabs[j].id})))
            }
            ActionId::TabsGoto(k) => {
                let t = self.tabs.get(k as usize - 1).map(|t| t.id).ok_or((code::NOT_FOUND, format!("no tab {k}")))?;
                self.active = Some(t);
                self.session_changed(fx);
                Ok(Some(json!({"tab": t})))
            }
            // The rest are the chrome's (dialogs, find bar, zoom, panels):
            // the window handles them before they reach the controller.
            _ => Err((code::UNAVAILABLE, format!("{} is handled by the window", action.id()))),
        }
    }

    /// Close a tab: `edit.close` (refused `dirty` when this is the last
    /// holder of unsaved text — plan D13); a detached or failed tab closes
    /// locally.
    fn close(&mut self, tab: TabId, force: bool, cmd: Option<(u64, String)>, intent: Intent, fx: &mut Vec<Effect>) {
        let buffer = self
            .tab(tab)
            .and_then(|t| t.mirror.as_ref())
            .filter(|m| !matches!(m.phase(), Phase::Detached { .. }))
            .map(|m| m.buffer().to_string());
        match buffer {
            Some(b) => {
                let out = Outgoing {
                    verb: "edit.close".into(),
                    body: json!({"buffer": b, "force": force, "origin": UI_ORIGIN}).to_string(),
                    op_id: None,
                    deadline_ms: DEADLINE_MS,
                };
                self.send(out, Req::Close { tab, cmd, intent }, fx);
            }
            None => {
                self.close_tab(tab, fx);
                if let Some((id, action)) = cmd {
                    fx.push(Effect::Respond { id, rc: 0, body: ok_body(&verbs::ActionReply { id: action, ok: true, result: None }) });
                }
            }
        }
    }

    // ── the ced.v1 port ──────────────────────────────────────────────────────

    fn bus_command(&mut self, cmd: &BusCommand, fx: &mut Vec<Effect>) {
        let body: Value = serde_json::from_str(&cmd.body).unwrap_or_else(|_| json!({}));
        macro_rules! parse {
            ($t:ty) => {
                match serde_json::from_value::<$t>(body.clone()) {
                    Ok(v) => v,
                    Err(e) => {
                        return fx.push(Effect::Respond {
                            id: cmd.id,
                            rc: 10,
                            body: refusal(code::INVALID_ARGUMENT, format!("{}: {e}", cmd.verb), Some("bad_args")),
                        });
                    }
                }
            };
        }
        let id = cmd.id;
        let reply = |fx: &mut Vec<Effect>, body: String| fx.push(Effect::Respond { id, rc: 0, body });
        let refuse = |fx: &mut Vec<Effect>, code: &str, msg: String, reason: Option<&str>| {
            fx.push(Effect::Respond { id, rc: 10, body: refusal(code, msg, reason) })
        };
        match cmd.verb.as_str() {
            "ced.ping" => reply(
                fx,
                ok_body(&verbs::PingReply {
                    pong: true,
                    service: verbs::SERVICE.into(),
                    schema: verbs::SCHEMA.into(),
                    pid: std::process::id(),
                    headless: self.headless,
                }),
            ),
            "ced.info" => {
                let info = cosmix_buildinfo::build_info!();
                reply(
                    fx,
                    ok_body(&verbs::InfoReply {
                        version: info.version.into(),
                        git_sha: info.git_sha.into(),
                        build_time: info.build_time.into(),
                        headless: self.headless,
                        tabs: self.tabs.len(),
                        edit: self.edit_info.clone(),
                        config_path: self.config_path.clone(),
                        session_path: self.session_path.clone(),
                    }),
                )
            }
            "ced.open" => {
                let r = parse!(verbs::OpenReq);
                if r.paths.is_empty() {
                    return refuse(fx, code::INVALID_ARGUMENT, "ced.open needs at least one path".into(), Some("bad_args"));
                }
                let goto = r.line.map(|l| (l, r.col.unwrap_or(1)));
                self.open_many(&r.paths, Some((id, goto)), fx);
            }
            "ced.new" => {
                let t = self.new_tab(None);
                self.send_open(t, None, false, fx);
                self.active = Some(t);
                self.opens.push(PendingOpen { cmd: id, tabs: vec![t], new: true });
                self.session_changed(fx);
            }
            "ced.tabs" => {
                let tabs = self.tabs.iter().map(|t| self.tab_row(t)).collect();
                reply(fx, ok_body(&verbs::TabsReply { active: self.active, tabs }));
            }
            "ced.focus" => {
                let sel = parse!(verbs::TabSel);
                match self.resolve_tab(&sel) {
                    Ok(t) => {
                        self.active = Some(t);
                        if let Some(x) = self.x.get_mut(&t) {
                            x.agent_since_focus = false;
                        }
                        self.session_changed(fx);
                        reply(fx, ok_body(&verbs::FocusReply { tab: t }));
                    }
                    Err(e) => refuse(fx, code::NOT_FOUND, e, None),
                }
            }
            "ced.state" => {
                let r = parse!(verbs::StateReq);
                match self.resolve_tab(&r.sel).map(|t| self.state_reply(t, r.text)) {
                    Ok(Ok(s)) => reply(fx, ok_body(&s)),
                    Ok(Err((c, m))) => refuse(fx, c, m, None),
                    Err(e) => refuse(fx, code::NOT_FOUND, e, None),
                }
            }
            "ced.type" => {
                let r = parse!(verbs::TypeReq);
                let t = match self.resolve_tab(&r.sel) {
                    Ok(t) => t,
                    Err(e) => return refuse(fx, code::NOT_FOUND, e, None),
                };
                match self.command(t, EditCommand::Insert(r.text), Intent::bus(t, &cmd.caller_key), fx) {
                    Ok(()) => {
                        let pending = self.tab(t).and_then(|t| t.mirror.as_ref()).map_or(0, Mirror::pending);
                        reply(fx, ok_body(&verbs::TypeReply { tab: t, pending }));
                    }
                    Err(e) => refuse(fx, code::CONFLICT, e, None),
                }
            }
            "ced.select" => {
                let r = parse!(verbs::SelectReq);
                let t = match self.resolve_tab(&r.sel) {
                    Ok(t) => t,
                    Err(e) => return refuse(fx, code::NOT_FOUND, e, None),
                };
                let res = self.tab(t).and_then(|t| t.mirror.as_ref()).ok_or("the buffer is not open".to_string()).and_then(|m| {
                    let a = resolve_pos(m.text(), &r.anchor)?;
                    let h = resolve_pos(m.text(), &r.head)?;
                    Ok((a, h, m.text().point(a), m.text().point(h)))
                });
                match res {
                    Ok((a, h, pa, ph)) => {
                        if let Some(t) = self.tab_mut(t) {
                            t.editor.sel = Selection { anchor: a, head: h };
                            t.editor.preferred_cells = None;
                        }
                        self.caret_moved(t, fx);
                        reply(fx, ok_body(&verbs::SelectReply { selection: verbs::SelectionP { anchor: pa, head: ph } }));
                    }
                    Err(e) => refuse(fx, code::INVALID_ARGUMENT, e, Some("bad_position")),
                }
            }
            "ced.action" => {
                let r = parse!(verbs::ActionReq);
                let Some(action) = ActionId::from_id(&r.id) else {
                    return refuse(fx, code::NOT_FOUND, format!("unknown action {}", r.id), Some("unknown_action"));
                };
                let tab = match (r.sel.tab, &r.sel.buffer) {
                    (None, None) => self.active,
                    _ => match self.resolve_tab(&r.sel) {
                        Ok(t) => Some(t),
                        Err(e) => return refuse(fx, code::NOT_FOUND, e, None),
                    },
                };
                let intent = Intent::bus(tab.unwrap_or(0), &cmd.caller_key);
                if !self.headless && ui::window_only(action, r.args.as_ref()) {
                    // Full mesh access: the window performs it, and the
                    // reply waits for the window's `ui_done`.
                    return self.ui_dispatch(tab, action, r.args.clone(), intent, Some((id, r.id.clone())), fx);
                }
                let arg = |k: &str| r.args.as_ref().and_then(|a| a.get(k)).and_then(Value::as_bool).unwrap_or(false);
                if action == ActionId::FileClose && !arg("save") {
                    match tab {
                        Some(t) => self.close(t, arg("force"), Some((id, r.id.clone())), intent, fx),
                        None => refuse(fx, code::NOT_FOUND, "no tab is open".into(), None),
                    }
                    return;
                }
                // One server op each: answered from its outcome (Opus m4).
                // `file.close {save:true}` answers when the save completes;
                // the close follows.
                let on_outcome = matches!(
                    action,
                    ActionId::FileSave
                        | ActionId::FileSaveAs
                        | ActionId::FileReload
                        | ActionId::FileClose
                        | ActionId::EditUndo
                        | ActionId::EditRedo
                        | ActionId::EditUndoAny
                        | ActionId::EditUndoOther
                );
                self.op_cmd = on_outcome.then(|| (id, r.id.clone()));
                let result = self.action_args(tab, action, r.args.as_ref(), intent, fx);
                let queued = on_outcome && self.op_cmd.take().is_none();
                match result {
                    Ok(_) if queued => {}
                    Ok(result) => reply(fx, ok_body(&verbs::ActionReply { id: r.id, ok: true, result })),
                    Err((c, m)) => {
                        self.op_waits.retain(|_, (i, _)| *i != id);
                        refuse(fx, c, m, None)
                    }
                }
            }
            "ced.actions" => {
                let actions = ActionId::all()
                    .into_iter()
                    .map(|a| verbs::ActionRow {
                        id: a.id(),
                        label: a.label(),
                        menu: a.menu().label().to_string(),
                        keys: crate::keymap::chords_for(a).into_iter().map(str::to_string).collect(),
                        enabled: true,
                    })
                    .collect();
                reply(fx, ok_body(&verbs::ActionsReply { actions }));
            }
            "ced.wait" => {
                let r = parse!(verbs::WaitReq);
                self.register_waiter(id, r, fx);
            }
            "ced.layout" => {
                if self.headless {
                    return refuse(fx, code::UNAVAILABLE, "ced.layout needs a window (this instance is --headless)".into(), Some("headless"));
                }
                let sel = parse!(verbs::TabSel);
                let t = match self.resolve_tab(&sel) {
                    Ok(t) => t,
                    Err(e) => return refuse(fx, code::NOT_FOUND, e, None),
                };
                if let Some(l) = self.layout.clone().filter(|l| l.tab == t) {
                    return reply(fx, ok_body(&l));
                }
                match self.x.get(&t).and_then(|x| x.layout) {
                    Some(l) => {
                        let rect = |r: [f32; 4]| verbs::Rect { x: r[0], y: r[1], w: r[2], h: r[3] };
                        let zero = verbs::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
                        reply(
                            fx,
                            ok_body(&verbs::LayoutReply {
                                tab: t,
                                window: zero,
                                menubar: zero,
                                tabstrip: zero,
                                editor: rect(l.editor),
                                gutter_w: l.gutter_w,
                                line_height: l.line_height,
                                cell_w: l.cell_w,
                                first_line: l.first_line,
                                visible_rows: l.visible_rows,
                                caret: rect(l.caret),
                                statusbar: zero,
                            }),
                        )
                    }
                    None => refuse(fx, code::UNAVAILABLE, "no frame has been laid out for this tab yet".into(), Some("no_frame")),
                }
            }
            "ced.stats" => {
                let s = &self.stats;
                reply(
                    fx,
                    ok_body(&verbs::StatsReply {
                        keys: s.keys,
                        frames: self.frames.count,
                        model_us: ui::percentiles(&self.frames.model_us),
                        view_us: ui::percentiles(&self.frames.view_us),
                        next_frame_us: ui::percentiles(&self.frames.next_frame_us),
                        events: s.events,
                        history_recoveries: s.history_recoveries,
                        snapshot_recoveries: s.snapshot_recoveries,
                        conflicts: s.conflicts,
                        retries: s.retries,
                        uncertain: s.uncertain,
                    }),
                )
            }
            "app.describe" => {
                let view = self
                    .active
                    .and_then(|a| self.tab(a))
                    .map(|t| self.tab_name(t))
                    .unwrap_or_default();
                reply(
                    fx,
                    ok_body(&verbs::DescribeReply {
                        contract: "ctk-app-control.v0".into(),
                        app: "ced".into(),
                        title: "CosMix Editor".into(),
                        view,
                        engine: "iced".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        description: "The CosMix Editor: an iced editor over the edit Bus service".into(),
                        controls: Vec::new(),
                        verbs: verbs::VERBS.iter().map(|(v, _)| v.to_string()).collect(),
                    }),
                )
            }
            "app.quit" => {
                reply(fx, ok_body(&verbs::QuitReply { quitting: true }));
                fx.push(Effect::SaveSession);
                fx.push(Effect::Quit);
            }
            "INFO" | "HELP" => {
                let verbs: Vec<&str> = verbs::VERBS.iter().map(|(v, _)| *v).collect();
                reply(fx, json!({"service": verbs::SERVICE, "schema": verbs::SCHEMA, "verbs": verbs}).to_string());
            }
            other => refuse(fx, code::UNKNOWN_VERB, format!("unknown verb {other}"), None),
        }
    }

    fn tab_name(&self, t: &Tab) -> String {
        t.mirror
            .as_ref()
            .and_then(|m| m.meta().name.clone())
            .or_else(|| t.path.as_ref().map(|p| p.rsplit('/').next().unwrap_or(p).to_string()))
            .unwrap_or_else(|| format!("untitled-{}", t.id))
    }

    fn tab_row(&self, t: &Tab) -> verbs::TabRow {
        let m = t.mirror.as_ref();
        verbs::TabRow {
            tab: t.id,
            buffer: m.map(|m| m.buffer().to_string()),
            epoch: m.map(|m| m.epoch().to_string()),
            path: t.path.clone(),
            name: self.tab_name(t),
            language: m.map_or_else(|| "text".into(), |m| m.meta().language.clone()),
            rev: m.map_or(0, Mirror::rev),
            dirty: m.is_some_and(|m| m.meta().dirty),
            disk: m.map_or_else(|| "none".into(), |m| disk_str(m.meta().disk)),
            pending: m.map_or(0, Mirror::pending),
            conflicts: m.map_or(0, |m| m.conflicts().len()),
            recovered: m.is_some_and(|m| m.meta().recovered),
            phase: self.phase_of(t.id),
        }
    }

    fn phase_of(&self, tab: TabId) -> PhaseW {
        let failed = self.x.get(&tab).is_some_and(|x| x.open_error.is_some());
        phase_w(self.tab(tab).and_then(|t| t.mirror.as_ref()), failed)
    }

    fn state_reply(&self, tab: TabId, with_text: bool) -> Result<verbs::StateReply, (&'static str, String)> {
        let t = self.tab(tab).ok_or((code::NOT_FOUND, format!("no tab {tab}")))?;
        let Some(m) = t.mirror.as_ref() else {
            let msg = self.x.get(&tab).and_then(|x| x.open_error.clone()).unwrap_or_else(|| "the buffer is still opening".into());
            return Err((code::UNAVAILABLE, msg));
        };
        let text = text_string(m.text());
        if with_text && text.len() > STATE_TEXT_MAX {
            return Err((code::INVALID_ARGUMENT, format!("the text is {} bytes (ced.state inlines at most {STATE_TEXT_MAX})", text.len())));
        }
        let len = m.text().len();
        let sel = t.editor.sel;
        Ok(verbs::StateReply {
            tab,
            buffer: Some(m.buffer().to_string()),
            rev: m.rev(),
            view_gen: m.view_gen(),
            phase: self.phase_of(tab),
            pending: m.pending(),
            inflight: m.inflight().is_some(),
            text_hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
            bytes: len,
            lines: m.text().line_count(),
            selection: verbs::SelectionP { anchor: m.text().point(sel.anchor.min(len)), head: m.text().point(sel.head.min(len)) },
            first_line: t.editor.scroll.first_line.max(1),
            last_remote: m.last_remote().map(|r| verbs::LastRemote {
                origin: r.origin.clone(),
                lane: r.lane.clone(),
                rev: r.rev,
                kind: kind_str(r.kind),
            }),
            conflicts: m
                .conflicts()
                .iter()
                .map(|c| verbs::ConflictRow { rev: c.rev, remote_origin: c.remote_origin.clone(), lines: [c.lines.0, c.lines.1], texts: c.texts.clone() })
                .collect(),
            detached_copy: m.detached_copy().is_some(),
            text: with_text.then_some(text),
        })
    }

    // ── ced.wait (plan §4.8): event-driven, never a loop ─────────────────────

    fn register_waiter(&mut self, cmd: u64, r: verbs::WaitReq, fx: &mut Vec<Effect>) {
        let refuse = |fx: &mut Vec<Effect>, code: &str, msg: String| fx.push(Effect::Respond { id: cmd, rc: 10, body: refusal(code, msg, Some("bad_args")) });
        let conds = [r.rev.map(WaitCond::Rev), r.idle.filter(|i| *i).map(|_| WaitCond::Idle), r.epoch.clone().map(WaitCond::Epoch), r.phase.map(WaitCond::Phase)];
        let mut given = conds.into_iter().flatten();
        let (Some(cond), None) = (given.next(), given.next()) else {
            return refuse(fx, code::INVALID_ARGUMENT, "ced.wait takes exactly one of rev / idle / epoch / phase".into());
        };
        if r.timeout_ms == 0 || r.timeout_ms > WAIT_MAX_MS {
            return refuse(fx, code::INVALID_ARGUMENT, format!("timeout_ms must be 1..={WAIT_MAX_MS}"));
        }
        let tab = match self.resolve_tab(&r.sel) {
            Ok(t) => t,
            Err(e) => return fx.push(Effect::Respond { id: cmd, rc: 10, body: refusal(code::NOT_FOUND, e, None) }),
        };
        self.next_waiter += 1;
        let w = Waiter { id: self.next_waiter, cmd, tab, cond, since: Instant::now(), timeout_ms: r.timeout_ms };
        let wid = w.id;
        self.waiters.push(w);
        // Evaluated at registration; replies at once when it already holds.
        self.eval_waiters(fx);
        if self.waiters.iter().any(|w| w.id == wid) {
            self.timer(r.timeout_ms, TimerFor::Wait(wid), fx);
        }
    }

    fn waiter_holds(&self, w: &Waiter) -> bool {
        let m = self.tab(w.tab).and_then(|t| t.mirror.as_ref());
        match &w.cond {
            WaitCond::Rev(r) => m.is_some_and(|m| m.rev() >= *r && matches!(m.phase(), Phase::Live)),
            WaitCond::Idle => m.is_some_and(Mirror::is_idle),
            WaitCond::Epoch(e) => m.is_some_and(|m| m.epoch() == e),
            WaitCond::Phase(p) => {
                let opened = self.x.get(&w.tab).is_some_and(|x| x.opened);
                (m.is_some() || opened) && self.phase_of(w.tab) == *p
            }
        }
    }

    /// Re-evaluate every waiter (after every controller transition).
    fn eval_waiters(&mut self, fx: &mut Vec<Effect>) {
        let ready: Vec<u64> = self.waiters.iter().filter(|w| self.waiter_holds(w)).map(|w| w.id).collect();
        for w in ready {
            self.finish_waiter(w, Ok(()), fx);
        }
    }

    fn finish_waiter(&mut self, wid: u64, outcome: Result<(), (&str, String)>, fx: &mut Vec<Effect>) {
        let Some(i) = self.waiters.iter().position(|w| w.id == wid) else { return };
        let w = self.waiters.remove(i);
        self.timers.retain(|_, t| !matches!(t, TimerFor::Wait(x) if *x == wid));
        let body = match outcome {
            Ok(()) => {
                let m = self.tab(w.tab).and_then(|t| t.mirror.as_ref());
                ok_body(&verbs::WaitReply {
                    tab: w.tab,
                    rev: m.map_or(0, Mirror::rev),
                    phase: self.phase_of(w.tab),
                    epoch: m.map(|m| m.epoch().to_string()),
                    waited_ms: w.since.elapsed().as_millis() as u64,
                })
            }
            Err((reason, msg)) => {
                return fx.push(Effect::Respond { id: w.cmd, rc: 10, body: refusal(code::CONFLICT, msg, Some(reason)) });
            }
        };
        fx.push(Effect::Respond { id: w.cmd, rc: 0, body });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_line_col_suffixes() {
        assert_eq!(split_line_col("/a/b.mix:12:3"), ("/a/b.mix".into(), Some((12, 3))));
        assert_eq!(split_line_col("/a/b.mix:12"), ("/a/b.mix".into(), Some((12, 1))));
        assert_eq!(split_line_col("/a/b.mix"), ("/a/b.mix".into(), None));
        assert_eq!(split_line_col("/a/b:c.mix"), ("/a/b:c.mix".into(), None));
        assert_eq!(split_line_col("/a/b:c.mix:7"), ("/a/b:c.mix".into(), Some((7, 1))));
    }

    #[test]
    fn line_col_uses_editd_columns() {
        let t = Text::from_text("ab\r\né x\n").unwrap();
        assert_eq!(offset_of_line_col(&t, 1, 1), Ok(0));
        assert_eq!(offset_of_line_col(&t, 1, 4), Ok(3)); // after the \r
        assert!(offset_of_line_col(&t, 1, 5).is_err());
        assert_eq!(offset_of_line_col(&t, 2, 2), Ok(6)); // é is 2 bytes
        assert_eq!(resolve_pos(&t, &PosSpec::Named(NamedPos::End)), Ok(t.len()));
        assert!(resolve_pos(&t, &PosSpec::Offset(5)).is_err()); // inside é
    }

    #[test]
    fn refusals_have_the_fixture_shape() {
        let v: Value = serde_json::from_str(&refusal(code::CONFLICT, "ced.wait timed out after 5000 ms", Some("timeout"))).unwrap();
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/verbs/refusal.timeout.json")).unwrap();
        assert_eq!(v, fixture);
    }

    fn ctl() -> Controller {
        Controller::new(Config::default(), 1, true)
    }

    fn cmd(verb: &str, body: Value) -> BusCommand {
        BusCommand { id: 7, verb: verb.into(), body: body.to_string(), caller_key: "local:tester".into() }
    }

    fn response(fx: &[Effect]) -> (u8, Value) {
        fx.iter()
            .find_map(|e| match e {
                Effect::Respond { rc, body, .. } => Some((*rc, serde_json::from_str(body).unwrap())),
                _ => None,
            })
            .expect("a response")
    }

    #[test]
    fn tabless_verbs_answer_without_a_tab() {
        let mut c = ctl();
        let (rc, v) = response(&c.on_bus_command(cmd("ced.ping", json!({}))));
        assert_eq!(rc, 0);
        assert_eq!(v["schema"], "ced.v1");
        assert_eq!(v["headless"], true);
        let (rc, v) = response(&c.on_bus_command(cmd("ced.tabs", json!({}))));
        assert_eq!((rc, v["tabs"].as_array().unwrap().len()), (0, 0));
        let (rc, v) = response(&c.on_bus_command(cmd("ced.layout", json!({}))));
        assert_eq!((rc, v["reason"].as_str()), (10, Some("headless")));
        let (rc, v) = response(&c.on_bus_command(cmd("ced.state", json!({}))));
        assert_eq!((rc, v["error_code"].as_str()), (10, Some("NOT_FOUND")));
        let (rc, v) = response(&c.on_bus_command(cmd("ced.wait", json!({"idle": true, "rev": 3, "timeout_ms": 10}))));
        assert_eq!((rc, v["reason"].as_str()), (10, Some("bad_args")));
        let (rc, v) = response(&c.on_bus_command(cmd("app.describe", json!({}))));
        assert_eq!((rc, v["contract"].as_str()), (0, Some("ctk-app-control.v0")));
        let (rc, v) = response(&c.on_bus_command(cmd("ced.actions", json!({}))));
        assert_eq!(rc, 0);
        assert!(v["actions"].as_array().unwrap().iter().any(|a| a["id"] == "edit.undo"));
        let (rc, _) = response(&c.on_bus_command(cmd("ced.nope", json!({}))));
        assert_eq!(rc, 10);
        let fx = c.on_bus_command(cmd("app.quit", json!({})));
        assert!(fx.contains(&Effect::Quit));
    }

    #[test]
    fn start_subscribes_and_asks_edit_its_epoch() {
        let mut c = ctl();
        let fx = c.start();
        for t in ["edit.changed", "theme.changed", "noded.props.changed"] {
            assert!(fx.contains(&Effect::Subscribe { topic: t.into() }), "{t}");
        }
        assert!(fx.iter().any(|e| matches!(e, Effect::Send { out, .. } if out.verb == "edit.info")));
    }

    fn sent(fx: &[Effect], verb: &str) -> (u64, Value) {
        fx.iter()
            .find_map(|e| match e {
                Effect::Send { req, out } if out.verb == verb => Some((*req, serde_json::from_str(&out.body).unwrap())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {verb} in {fx:?}"))
    }

    fn reply(c: &mut Controller, req: u64, body: Value) -> Vec<Effect> {
        c.on_incoming(Incoming::Reply { req, rc: 0, body: body.to_string() })
    }

    /// Open a tab on a hand-driven `edit`, type over the Bus, ack by the echo.
    #[test]
    fn bus_typing_uses_the_callers_lane_and_acks_by_echo() {
        let mut c = ctl();
        c.start();
        let fx = c.on_bus_command(cmd("ced.open", json!({"paths": ["/nonexistent/ced-e1d/x.txt"]})));
        let (req, _) = sent(&fx, "edit.open");
        let open = json!({"buffer": "b1_0000e1e1", "epoch": "0000e1e1", "path": "/nonexistent/ced-e1d/x.txt", "opened_as": null,
                          "name": "x.txt", "language": "text", "rev": 0, "lines": 2, "bytes": 12, "eol": "lf", "bom": false,
                          "disk": "clean", "reopened": false, "created": false, "recovery_id": "5f0c2a9e1b7d4c33",
                          "recovered": false, "recovered_from": null});
        let fx = reply(&mut c, req, open);
        let (rc, v) = response(&fx);
        assert_eq!((rc, v["tabs"][0]["buffer"].as_str()), (0, Some("b1_0000e1e1")));
        let (req, get) = sent(&fx, "edit.get");
        assert_eq!(get["snapshot"], true);
        let p = |o: usize, col: usize| json!({"offset": o, "line": 1, "col": col});
        let page = json!({"buffer": "b1_0000e1e1", "epoch": "0000e1e1", "rev": 0, "text": "hello world\n", "lines": null,
                          "start": p(0, 1), "end": p(12, 13), "bytes_total": 12, "lines_total": 2, "truncated": false,
                          "next": null, "snapshot": "s1"});
        reply(&mut c, req, page);
        let (rc, v) = response(&c.on_bus_command(cmd("ced.wait", json!({"phase": "live", "timeout_ms": 1000}))));
        assert_eq!((rc, v["phase"].as_str()), (0, Some("live")));

        let fx = c.on_bus_command(cmd("ced.type", json!({"text": "!"})));
        let (rc, v) = response(&fx);
        assert_eq!((rc, v["pending"].as_u64()), (0, Some(1)));
        let (req, ins) = sent(&fx, "edit.insert");
        assert_eq!(ins["origin"], "agent:ced.local_tester", "a Bus caller types in its own lane");
        assert_eq!((ins["at"].as_u64(), ins["base_rev"].as_u64()), (Some(0), Some(0)));
        let op_id = ins["op_id"].as_str().unwrap().to_string();
        let (rc, v) = response(&c.on_bus_command(cmd("ced.state", json!({"text": true}))));
        assert_eq!((rc, v["text"].as_str(), v["inflight"].as_bool()), (0, Some("!hello world\n"), Some(true)));
        assert_eq!(v["selection"]["head"]["offset"].as_u64(), Some(1), "caret_after of the issuing view");

        reply(&mut c, req, json!({"buffer": "b1_0000e1e1", "epoch": "0000e1e1", "rev": 1, "op_id": op_id}));
        let ev = json!({"event": "edit", "epoch": "0000e1e1", "buffer": "b1_0000e1e1", "rev": 1, "base_rev": 0,
                        "origin": "agent:ced.local_tester", "lane": "agent:ced.local_tester", "kind": "edit", "of": null,
                        "op_id": op_id, "edits": [{"offset": 0, "delete": 0, "insert": "!"}], "event_seq": 1});
        c.on_incoming(Incoming::Topic { topic: "edit.changed".into(), body: ev.to_string() });
        let (rc, v) = response(&c.on_bus_command(cmd("ced.wait", json!({"idle": true, "timeout_ms": 1000}))));
        assert_eq!((rc, v["rev"].as_u64()), (0, Some(1)));
        let (_, v) = response(&c.on_bus_command(cmd("ced.state", json!({}))));
        assert_eq!(v["text_hash"].as_str().unwrap(), blake3::hash(b"!hello world\n").to_hex().as_str());
        assert_eq!((v["pending"].as_u64(), v["inflight"].as_bool()), (Some(0), Some(false)));

        // A window-only action over the Bus: headless refuses; with a window
        // the reply waits for ui_done, or times out.
        let (rc, v) = response(&c.on_bus_command(cmd("ced.action", json!({"id": "view.zoom_in"}))));
        assert_eq!((rc, v["error_code"].as_str()), (10, Some("UNAVAILABLE")));
        let mut gui = Controller::new(Config::default(), 2, false);
        let fx = gui.on_bus_command(cmd("ced.action", json!({"id": "search.find"})));
        let token = fx
            .iter()
            .find_map(|e| match e {
                Effect::UiAction { action: ActionId::SearchFind, token, .. } => Some(*token),
                _ => None,
            })
            .expect("a UiAction");
        assert!(!fx.iter().any(|e| matches!(e, Effect::Respond { .. })), "no reply before the window acts");
        let fx = gui.ui_done(token, Ok(json!({"opened": "find"})));
        let (rc, v) = response(&fx);
        assert_eq!((rc, v["ok"].as_bool(), v["result"]["opened"].as_str()), (0, Some(true), Some("find")));
        assert!(gui.ui_done(token, Ok(Value::Null)).is_empty(), "a token answers once");
        let fx = gui.on_bus_command(cmd("ced.action", json!({"id": "help.about"})));
        let timer = fx
            .iter()
            .find_map(|e| match e {
                Effect::Timer { id, ms } if *ms == 10_000 => Some(*id),
                _ => None,
            })
            .expect("a deadline");
        let (rc, v) = response(&gui.on_incoming(Incoming::Timer { id: timer }));
        assert_eq!((rc, v["error_code"].as_str(), v["reason"].as_str()), (10, Some("TIMEOUT"), Some("timeout")));
    }

    const B: &str = "b1_0000e1e1";

    /// A headless controller with one live tab on "hello world\n" (rev 0);
    /// returns the `edit.list` request the attach sent for the save state.
    fn live(c: &mut Controller) -> u64 {
        c.start();
        let fx = c.on_bus_command(cmd("ced.open", json!({"paths": ["/nonexistent/ced-r/x.txt"]})));
        let (req, _) = sent(&fx, "edit.open");
        let open = json!({"buffer": B, "epoch": "0000e1e1", "path": "/nonexistent/ced-r/x.txt", "opened_as": null,
                          "name": "x.txt", "language": "text", "rev": 0, "lines": 2, "bytes": 12, "eol": "lf", "bom": false,
                          "disk": "clean", "reopened": false, "created": false, "recovery_id": "5f0c2a9e1b7d4c33",
                          "recovered": false, "recovered_from": null});
        let fx = reply(c, req, open);
        let (list, _) = sent(&fx, "edit.list");
        let (req, _) = sent(&fx, "edit.get");
        let p = |o: usize, col: usize| json!({"offset": o, "line": 1, "col": col});
        let page = json!({"buffer": B, "epoch": "0000e1e1", "rev": 0, "text": "hello world\n", "lines": null,
                          "start": p(0, 1), "end": p(12, 13), "bytes_total": 12, "lines_total": 2, "truncated": false,
                          "next": null, "snapshot": "s1"});
        reply(c, req, page);
        list
    }

    fn refused_reply(c: &mut Controller, req: u64, code: &str, reason: &str) -> Vec<Effect> {
        let body = json!({"error_code": code, "message": format!("refused: {reason}"), "reason": reason, "buffer": B, "rev": 0});
        c.on_incoming(Incoming::Reply { req, rc: 10, body: body.to_string() })
    }

    #[test]
    fn bus_actions_answer_from_the_server_ops_outcome() {
        // Opus m4: not `ok:true` at enqueue.
        let mut c = ctl();
        live(&mut c);
        let fx = c.on_bus_command(cmd("ced.action", json!({"id": "edit.undo"})));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Respond { .. })), "no answer before the service's");
        let (req, _) = sent(&fx, "edit.undo");
        let (rc, v) = response(&refused_reply(&mut c, req, "NOT_FOUND", "nothing_to_undo"));
        assert_eq!((rc, v["error_code"].as_str(), v["reason"].as_str()), (10, Some("NOT_FOUND"), Some("nothing_to_undo")));

        let fx = c.on_bus_command(cmd("ced.action", json!({"id": "file.save"})));
        assert!(!fx.iter().any(|e| matches!(e, Effect::Respond { .. })));
        let (req, _) = sent(&fx, "edit.save");
        // A Bus caller's disk_modified goes to the caller, never a prompt.
        let fx = refused_reply(&mut c, req, "CONFLICT", "disk_modified");
        assert!(!fx.iter().any(|e| matches!(e, Effect::Prompt(_))), "no prompt for a Bus save: {fx:?}");
        let (rc, v) = response(&fx);
        assert_eq!((rc, v["reason"].as_str()), (10, Some("disk_modified")));

        let fx = c.on_bus_command(cmd("ced.action", json!({"id": "file.save"})));
        let (req, _) = sent(&fx, "edit.save");
        let saved = json!({"buffer": B, "epoch": "0000e1e1", "path": "/nonexistent/ced-r/x.txt", "rev": 0, "saved_rev": 0,
                           "file_bytes": 12, "disk": "clean", "durable": true, "warning": null});
        let (rc, v) = response(&reply(&mut c, req, saved));
        assert_eq!((rc, v["id"].as_str(), v["ok"].as_bool()), (0, Some("file.save"), Some(true)));
    }

    #[test]
    fn a_lost_echo_after_the_reply_recovers_from_history() {
        // Opus M1: the mirror's echo timer reaches the host and, when it
        // fires with the op still waiting, starts a history recovery.
        let mut c = ctl();
        live(&mut c);
        let fx = c.on_bus_command(cmd("ced.type", json!({"text": "!"})));
        let (req, ins) = sent(&fx, "edit.insert");
        let fx = reply(&mut c, req, json!({"buffer": B, "epoch": "0000e1e1", "rev": 1, "op_id": ins["op_id"]}));
        let timer = fx
            .iter()
            .find_map(|e| match e {
                Effect::Timer { id, ms: 5_000 } => Some(*id),
                _ => None,
            })
            .expect("an echo timer with the reply");
        // No event ever comes; the timer fires.
        let fx = c.on_incoming(Incoming::Timer { id: timer });
        let (_, h) = sent(&fx, "edit.history");
        assert_eq!((h["buffer"].as_str(), h["since_rev"].as_u64()), (Some(B), Some(0)));
    }

    #[test]
    fn find_replies_wait_for_pending_local_edits() {
        // GLM M1: a reply at the mirror's rev is still the service's text,
        // not the view, while a local edit is pending.
        let mut c = ctl();
        live(&mut c);
        let fx = c.on_bus_command(cmd("ced.action", json!({"id": "search.find_next", "args": {"pattern": "world"}})));
        // The find job's request (`from`), not highlight-all's.
        let find_req = |fx: &[Effect]| {
            fx.iter().find_map(|e| match e {
                Effect::Send { req, out } if out.verb == "edit.find" && out.body.contains("\"from\"") => Some(*req),
                _ => None,
            })
        };
        let find = find_req(&fx).expect("the find job's edit.find");
        let fx = c.on_bus_command(cmd("ced.type", json!({"text": "!"})));
        let (req, ins) = sent(&fx, "edit.insert");
        let pt = |o: usize| json!({"offset": o, "line": 1, "col": o + 1});
        let m = json!({"start": pt(6), "end": pt(11), "text": "world", "text_truncated": false, "groups": null, "groups_truncated": false});
        let fx = reply(&mut c, find, json!({"buffer": B, "rev": 0, "matches": [m], "truncated": false, "next": null}));
        assert!(find_req(&fx).is_none(), "waits for idle");
        let (_, st) = response(&c.on_bus_command(cmd("ced.state", json!({}))));
        assert_eq!((st["selection"]["anchor"]["offset"].as_u64(), st["selection"]["head"]["offset"].as_u64()), (Some(1), Some(1)), "unmapped offsets not applied");
        reply(&mut c, req, json!({"buffer": B, "epoch": "0000e1e1", "rev": 1, "op_id": ins["op_id"]}));
        let ev = json!({"event": "edit", "epoch": "0000e1e1", "buffer": B, "rev": 1, "base_rev": 0, "origin": "agent:ced.local_tester",
                        "lane": "agent:ced.local_tester", "kind": "edit", "of": null, "op_id": ins["op_id"],
                        "edits": [{"offset": 0, "delete": 0, "insert": "!"}], "event_seq": 1});
        let fx = c.on_incoming(Incoming::Topic { topic: "edit.changed".into(), body: ev.to_string() });
        assert!(find_req(&fx).is_some(), "asked again once idle");
    }

    #[test]
    fn the_save_state_comes_from_the_list_row_and_keys_feed_model_us() {
        // Opus m2: another holder's unsaved edits show as dirty.
        let mut c = ctl();
        let list = live(&mut c);
        let (_, v) = response(&c.on_bus_command(cmd("ced.tabs", json!({}))));
        assert_eq!(v["tabs"][0]["dirty"].as_bool(), Some(false), "the open-time guess");
        let row = json!({"buffer": B, "path": "/nonexistent/ced-r/x.txt", "opened_as": null, "name": "x.txt", "language": "text",
                         "rev": 0, "saved_rev": null, "dirty": true, "disk": "clean", "lines": 2, "bytes": 12,
                         "holders": ["local:other"], "recovery_id": "5f0c2a9e1b7d4c33", "recovered": false});
        reply(&mut c, list, json!({"epoch": "0000e1e1", "buffers": [row]}));
        let (_, v) = response(&c.on_bus_command(cmd("ced.tabs", json!({}))));
        assert_eq!(v["tabs"][0]["dirty"].as_bool(), Some(true));
        // A disk-state change asks again.
        let ev = json!({"event": "disk", "epoch": "0000e1e1", "buffer": B, "rev": 0, "disk": "modified", "event_seq": 1});
        let fx = c.on_incoming(Incoming::Topic { topic: "edit.changed".into(), body: ev.to_string() });
        sent(&fx, "edit.list");
        // ced.stats model_us is measured per key.
        let tab = v["tabs"][0]["tab"].as_u64().unwrap() as TabId;
        c.on_editor(tab, EditorMsg::Command(EditCommand::Insert("x".into())));
        assert_eq!(c.frames.model_us.len(), 1);
    }

    #[test]
    fn recovered_buffers_prompt_once_and_frames_feed_stats() {
        let mut c = ctl();
        let fx = c.start();
        let req = fx
            .iter()
            .find_map(|e| match e {
                Effect::Send { req, out } if out.verb == "edit.list" => Some(*req),
                _ => None,
            })
            .expect("edit.list at start");
        let row = |b: &str, recovered: bool, holders: Vec<String>| {
            json!({"buffer": b, "path": null, "opened_as": null, "name": null, "language": "text", "rev": 0,
                   "saved_rev": null, "dirty": true, "disk": "none", "lines": 1, "bytes": 5, "holders": holders,
                   "recovery_id": "5f0c2a9e1b7d4c33", "recovered": recovered})
        };
        let body = json!({"epoch": "0000e1e1", "buffers": [row("b1_0000e1e1", true, vec![]), row("b2_0000e1e1", false, vec![]),
                                                            row("b3_0000e1e1", true, vec!["local:x".into()])]});
        let fx = c.on_incoming(Incoming::Reply { req, rc: 0, body: body.to_string() });
        let prompts: Vec<_> = fx.iter().filter_map(|e| match e { Effect::Prompt(p) => Some(p.clone()), _ => None }).collect();
        assert_eq!(prompts.len(), 1);
        let Prompt::Recovered { buffers } = &prompts[0] else { panic!("{prompts:?}") };
        assert_eq!(buffers.iter().map(|b| b.buffer.as_str()).collect::<Vec<_>>(), ["b1_0000e1e1"]);
        let fx = c.discard_recovered("b1_0000e1e1");
        assert!(fx.iter().any(|e| matches!(e, Effect::Send { out, .. } if out.verb == "edit.close" && out.body.contains("\"force\":true"))));
        for v in 1..=100 {
            c.record_frame(v, Some(v * 10));
        }
        let (rc, v) = response(&c.on_bus_command(cmd("ced.stats", json!({}))));
        assert_eq!(rc, 0);
        assert_eq!((v["frames"].as_u64(), v["view_us"]["p99"].as_u64(), v["next_frame_us"]["max"].as_u64()), (Some(100), Some(99), Some(1000)));
        assert!(c.edit_info().is_none());
    }
}
