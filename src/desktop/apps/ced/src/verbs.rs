//! The `ced` Bus port, schema `ced.v1` (ced E1 plan §4.8) — manifest and every
//! request/reply DTO, **complete and frozen in Stage S**; golden fixtures in
//! `tests/fixtures/verbs/` (checked by `tests/verb_fixtures.rs`).
//!
//! All verbs are reachable by local and mesh callers with no authorization
//! gate (the full-mesh-access law). Success = rc 0; refusal = rc 10 with a
//! [`Refusal`] body. Edits caused by a Bus caller claim
//! `agent:<cosmix_edit_client::types::bus_lane_label(caller_key)>`; `ced.type`
//! and `ced.select` act on the window's (Mark's) selection — ARexx-style, by
//! design.

use serde::{Deserialize, Serialize};

pub const SERVICE: &str = "ced";
pub const SCHEMA: &str = "ced.v1";

/// `(verb, read_only)` in manifest order.
pub const VERBS: &[(&str, bool)] = &[
    ("ced.ping", true),
    ("ced.info", true),
    ("ced.open", false),
    ("ced.new", false),
    ("ced.tabs", true),
    ("ced.focus", false),
    ("ced.state", true),
    ("ced.type", false),
    ("ced.select", false),
    ("ced.action", false),
    ("ced.actions", true),
    ("ced.wait", true),
    ("ced.layout", true),
    ("ced.stats", true),
    ("app.describe", true),
    ("app.quit", false),
];

/// Refusal codes (`error_code`).
pub mod code {
    pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const CONFLICT: &str = "CONFLICT";
    pub const UNAVAILABLE: &str = "UNAVAILABLE";
    pub const INTERNAL: &str = "INTERNAL";
    pub const UNKNOWN_VERB: &str = "UNKNOWN_VERB";
}

/// Every refusal body (decision 10 shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub error_code: String,
    pub message: String,
    pub reason: Option<String>,
}

/// `POINT` as in the `edit` wire (1-based line/col, editd col semantics).
pub use cosmix_edit_core::pos::Point;

/// A tab: by id, or by the buffer it shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSel {
    #[serde(default)]
    pub tab: Option<u64>,
    #[serde(default)]
    pub buffer: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyReq {}

// ── ping / info ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingReply {
    pub pong: bool,
    pub service: String,
    pub schema: String,
    pub pid: u32,
    pub headless: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditInfo {
    pub epoch: Option<String>,
    pub version: Option<String>,
    pub volatile: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoReply {
    pub version: String,
    pub git_sha: String,
    pub build_time: String,
    pub headless: bool,
    pub tabs: usize,
    pub edit: EditInfo,
    pub config_path: Option<String>,
    pub session_path: Option<String>,
}

// ── open / new / tabs / focus ───────────────────────────────────────────────

/// `paths` accept a `path:line[:col]` suffix when that file does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReq {
    pub paths: Vec<String>,
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub col: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenedTab {
    pub tab: u64,
    pub buffer: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    pub tabs: Vec<OpenedTab>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewReply {
    pub tab: u64,
    pub buffer: Option<String>,
}

/// `live | bootstrapping | recovering | detached`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseW {
    Bootstrapping,
    Live,
    Recovering,
    Detached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabRow {
    pub tab: u64,
    pub buffer: Option<String>,
    pub epoch: Option<String>,
    pub path: Option<String>,
    pub name: String,
    pub language: String,
    pub rev: u64,
    pub dirty: bool,
    pub disk: String,
    pub pending: usize,
    pub conflicts: usize,
    pub recovered: bool,
    pub phase: PhaseW,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabsReply {
    pub active: Option<u64>,
    pub tabs: Vec<TabRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusReply {
    pub tab: u64,
}

// ── state / type / select ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateReq {
    #[serde(flatten)]
    pub sel: TabSel,
    /// Include the view text (refused above 4 MiB).
    #[serde(default)]
    pub text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionP {
    pub anchor: Point,
    pub head: Point,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRemote {
    pub origin: String,
    pub lane: String,
    pub rev: u64,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRow {
    pub rev: u64,
    pub remote_origin: Option<String>,
    pub lines: [usize; 2],
    pub texts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateReply {
    pub tab: u64,
    pub buffer: Option<String>,
    pub rev: u64,
    pub view_gen: u64,
    pub phase: PhaseW,
    pub pending: usize,
    pub inflight: bool,
    /// 64 lowercase hex of blake3(view text).
    pub text_hash: String,
    pub bytes: usize,
    pub lines: usize,
    pub selection: SelectionP,
    pub first_line: usize,
    pub last_remote: Option<LastRemote>,
    pub conflicts: Vec<ConflictRow>,
    pub detached_copy: bool,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeReq {
    pub text: String,
    #[serde(flatten)]
    pub sel: TabSel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeReply {
    pub tab: u64,
    pub pending: usize,
}

/// `anchor` / `head`: a byte offset or `{line, col}` (editd POS forms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectReq {
    pub anchor: cosmix_edit_core::pos::PosSpec,
    pub head: cosmix_edit_core::pos::PosSpec,
    #[serde(flatten)]
    pub sel: TabSel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectReply {
    pub selection: SelectionP,
}

// ── actions ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReq {
    pub id: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(flatten)]
    pub sel: TabSel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReply {
    pub id: String,
    pub ok: bool,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRow {
    pub id: String,
    pub label: String,
    pub menu: String,
    pub keys: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsReply {
    pub actions: Vec<ActionRow>,
}

// ── wait ────────────────────────────────────────────────────────────────────

/// Exactly one condition. Event-driven: evaluated at registration and after
/// every controller transition; a one-shot deadline; cancelled on tab close
/// or Bus loss (`CONFLICT reason:"cancelled"`); timeout → `CONFLICT
/// reason:"timeout"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitReq {
    #[serde(flatten)]
    pub sel: TabSel,
    #[serde(default)]
    pub rev: Option<u64>,
    #[serde(default)]
    pub idle: Option<bool>,
    #[serde(default)]
    pub epoch: Option<String>,
    #[serde(default)]
    pub phase: Option<PhaseW>,
    /// ≤ 30000.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitReply {
    pub tab: u64,
    pub rev: u64,
    pub phase: PhaseW,
    pub epoch: Option<String>,
    pub waited_ms: u64,
}

// ── layout / stats ──────────────────────────────────────────────────────────

/// A rectangle in logical px.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Engine geometry of the last drawn frame (headless → `UNAVAILABLE
/// reason:"headless"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutReply {
    pub tab: u64,
    pub window: Rect,
    pub menubar: Rect,
    pub tabstrip: Rect,
    pub editor: Rect,
    pub gutter_w: f32,
    pub line_height: f32,
    pub cell_w: f32,
    pub first_line: usize,
    pub visible_rows: usize,
    pub caret: Rect,
    pub statusbar: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Percentiles {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsReply {
    pub keys: u64,
    pub frames: u64,
    pub model_us: Percentiles,
    pub view_us: Percentiles,
    pub next_frame_us: Percentiles,
    pub events: u64,
    pub history_recoveries: u64,
    pub snapshot_recoveries: u64,
    pub conflicts: u64,
    pub retries: u64,
    pub uncertain: u64,
}

// ── app control (ctk-app-control.v0, ctk/src/app_control.rs:733-762) ─────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeReply {
    pub contract: String,
    pub app: String,
    pub title: String,
    pub view: String,
    pub engine: String,
    pub version: String,
    pub description: String,
    pub controls: Vec<serde_json::Value>,
    pub verbs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuitReply {
    pub quitting: bool,
}
