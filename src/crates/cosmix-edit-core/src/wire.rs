//! The `edit.v1` Bus contract as serde types (plan §4). One request and one
//! reply type per verb, one type per event kind, and the refusal body.
//!
//! Golden fixtures: `tests/fixtures/contract/*.json`, checked by
//! `tests/contract_fixtures.rs` (requests must parse; replies, refusals and
//! events must round-trip exactly).
//!
//! Conventions: args are a JSON object (Mix `send` kv → JSON body). Never
//! name an arg `id`, `body` or `namespace` (envelope id / Mix header routing).
//! Replies serialize every field (absent = `null`) so fixtures are exact.
//! `BufferId` = `"b<N>_<epoch>"`, epoch = 8 lowercase hex, fresh per daemon start.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use crate::buffer::{Eol, OpSpec};
pub use crate::error::ErrorCode;
pub use crate::ot::Edit;
pub use crate::pos::{AllTag, NamedPos, Point, PosSpec, RangeSpec, SelSpec};
pub use crate::anchor::Bias;

pub type BufferId = String;

pub const SERVICE: &str = "edit";
pub const SCHEMA: &str = "edit.v1";
pub const TOPIC_CHANGED: &str = "edit.changed";
pub const TOPIC_PROPS_CHANGED: &str = "edit.props.changed";

/// Every verb, in manifest order. `read_only` per the manifest (plan §4.4).
pub const VERBS: &[(&str, bool)] = &[
    ("edit.ping", true),
    ("edit.info", true),
    ("edit.list", true),
    ("edit.open", false),
    ("edit.close", false),
    ("edit.save", false),
    ("edit.reload", false),
    ("edit.get", true),
    ("edit.insert", false),
    ("edit.delete", false),
    ("edit.replace", false),
    ("edit.apply", false),
    ("edit.find", true),
    ("edit.select", false),
    ("edit.cursor", false),
    ("edit.anchor.set", false),
    ("edit.anchor.get", true),
    ("edit.anchor.clear", false),
    ("edit.undo", false),
    ("edit.redo", false),
    ("edit.history", true),
    ("edit.props.get", true),
    ("edit.props.list", true),
    ("edit.props.describe", true),
    ("edit.props.watch", true),
];

// ── shared request fragments ────────────────────────────────────────────────

/// Label claim + retry id, on every mutating verb.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutMeta {
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub op_id: Option<String>,
}

/// At most one of the two (both → INVALID_ARGUMENT `both_cas`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CasArgs {
    #[serde(default)]
    pub expect_rev: Option<u64>,
    #[serde(default)]
    pub base_rev: Option<u64>,
}

/// Fields common to the four text mutations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMutArgs {
    #[serde(flatten)]
    pub cas: CasArgs,
    #[serde(default)]
    pub coalesce: bool,
    /// Post-edit coordinates.
    #[serde(default)]
    pub cursor: Option<PosSpec>,
    #[serde(flatten)]
    pub meta: MutMeta,
}

// ── requests ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyReq {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReq {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub force: bool,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub expect_rev: Option<u64>,
    #[serde(default)]
    pub force: bool,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub expect_rev: Option<u64>,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapshotArg {
    /// `true` on the first page: freeze a snapshot.
    Start(bool),
    /// `"s<N>"` on later pages.
    Token(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReq {
    pub buffer: BufferId,
    /// Default `"all"`.
    #[serde(default)]
    pub range: Option<RangeSpec>,
    #[serde(default)]
    pub numbered: bool,
    #[serde(default)]
    pub expect_rev: Option<u64>,
    #[serde(default)]
    pub snapshot: Option<SnapshotArg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertReq {
    pub buffer: BufferId,
    pub at: PosSpec,
    pub text: String,
    #[serde(flatten)]
    pub args: TextMutArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReq {
    pub buffer: BufferId,
    pub range: RangeSpec,
    #[serde(flatten)]
    pub args: TextMutArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceReq {
    pub buffer: BufferId,
    pub range: RangeSpec,
    pub text: String,
    #[serde(flatten)]
    pub args: TextMutArgs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReq {
    pub buffer: BufferId,
    pub ops: Vec<OpSpec>,
    #[serde(flatten)]
    pub args: TextMutArgs,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindReq {
    pub buffer: BufferId,
    pub pattern: String,
    #[serde(default)]
    pub regex: bool,
    /// `true` = case-sensitive (default).
    #[serde(default = "default_true")]
    pub case: bool,
    #[serde(default)]
    pub range: Option<RangeSpec>,
    #[serde(default)]
    pub groups: bool,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub from: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectReq {
    pub buffer: BufferId,
    pub ranges: Vec<SelSpec>,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorReq {
    pub buffer: BufferId,
    pub at: PosSpec,
    #[serde(flatten)]
    pub meta: MutMeta,
}

/// Exactly one of `at` / `range`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorSetReq {
    pub buffer: BufferId,
    pub name: String,
    #[serde(default)]
    pub at: Option<PosSpec>,
    #[serde(default)]
    pub range: Option<RangeSpec>,
    #[serde(default)]
    pub bias: Option<Bias>,
    #[serde(flatten)]
    pub meta: MutMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorGetReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorClearReq {
    pub buffer: BufferId,
    pub name: String,
    #[serde(flatten)]
    pub meta: MutMeta,
}

/// `edit.undo` and `edit.redo`. Here `origin` selects the LANE (`"*"` = global,
/// default = the caller's own) and `as` is the caller's label claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default, rename = "as")]
    pub as_: Option<String>,
    #[serde(default)]
    pub expect_rev: Option<u64>,
    #[serde(default)]
    pub op_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryReq {
    pub buffer: BufferId,
    #[serde(default)]
    pub since_rev: u64,
    #[serde(default)]
    pub limit: Option<usize>,
}

// ── replies ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingReply {
    pub pong: bool,
    pub service: String,
    pub schema: String,
    pub epoch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InfoReply {
    pub name: String,
    pub schema: String,
    pub epoch: String,
    pub props_level: String,
    pub binary: String,
    pub version: String,
    pub git_sha: String,
    pub git_dirty: bool,
    pub build_time: String,
    pub buffers: usize,
    pub dirty: usize,
    pub volatile: bool,
    pub mesh_open: bool,
    pub event_seq: u64,
    pub publisher_loss: u64,
    pub limits: Map<String, Value>,
}

/// `disk` ∈ `clean | modified | deleted | none | unwatched`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskState {
    Clean,
    Modified,
    Deleted,
    None,
    Unwatched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferSummary {
    pub buffer: BufferId,
    pub path: Option<String>,
    pub opened_as: Option<String>,
    pub name: Option<String>,
    pub language: String,
    pub rev: u64,
    pub saved_rev: Option<u64>,
    pub dirty: bool,
    pub disk: DiskState,
    pub lines: usize,
    /// Text bytes, BOM excluded (everywhere).
    pub bytes: usize,
    pub holders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListReply {
    pub epoch: String,
    pub buffers: Vec<BufferSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    pub buffer: BufferId,
    pub epoch: String,
    pub path: Option<String>,
    pub opened_as: Option<String>,
    pub name: Option<String>,
    pub language: String,
    pub rev: u64,
    pub lines: usize,
    pub bytes: usize,
    pub eol: Eol,
    pub bom: bool,
    pub disk: DiskState,
    pub reopened: bool,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseReply {
    pub buffer: BufferId,
    pub closed: bool,
    pub holders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveReply {
    pub buffer: BufferId,
    pub epoch: String,
    pub path: String,
    pub rev: u64,
    pub saved_rev: u64,
    pub file_bytes: usize,
    pub disk: DiskState,
    /// `false` when the rename committed but the directory fsync failed.
    pub durable: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberedLine {
    pub line: usize,
    pub text: String,
    /// `true` on every chunk after the first of a line split across pages.
    pub cont: bool,
}

/// Exactly one of `text` / `lines` is non-null (`numbered` selects).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetReply {
    pub buffer: BufferId,
    pub epoch: String,
    pub rev: u64,
    pub text: Option<String>,
    pub lines: Option<Vec<NumberedLine>>,
    pub start: Point,
    pub end: Point,
    pub bytes_total: usize,
    pub lines_total: usize,
    pub truncated: bool,
    pub next: Option<usize>,
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: Point,
    pub end: Point,
}

/// Compact mutation reply (plan §4.4): no per-edit array, at most
/// `REPLY_CHANGED_MAX` spans. Also the base of undo/redo/reload replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationReply {
    pub buffer: BufferId,
    pub epoch: String,
    pub rev: u64,
    pub base_rev: u64,
    pub origin: String,
    pub origin_downgraded: bool,
    pub op_id: Option<String>,
    pub duplicate: bool,
    pub rebased: bool,
    pub edit_count: usize,
    pub inserted_bytes: usize,
    pub deleted_bytes: usize,
    /// Envelope of all changed spans; `null` for a pure delete.
    pub changed_span: Option<Span>,
    pub changed: Vec<Span>,
    pub changed_truncated: bool,
    pub cursor: Option<Point>,
    pub dirty: bool,
    pub lines: usize,
    pub bytes: usize,
    pub history_trimmed_to: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoReply {
    #[serde(flatten)]
    pub mutation: MutationReply,
    pub lane: String,
    /// `[first_rev, last_rev]` undone (undo) or redone (redo).
    pub undid: Option<[u64; 2]>,
    pub redid: Option<[u64; 2]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnchangedReply {
    pub buffer: BufferId,
    pub rev: u64,
    pub unchanged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReloadReply {
    Applied(Box<MutationReply>),
    Unchanged(UnchangedReply),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchW {
    pub start: Point,
    pub end: Point,
    pub text: String,
    pub text_truncated: bool,
    pub groups: Option<Vec<Option<String>>>,
    pub groups_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindReply {
    pub buffer: BufferId,
    pub rev: u64,
    pub matches: Vec<MatchW>,
    pub truncated: bool,
    pub next: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionW {
    pub anchor: Point,
    pub head: Point,
}

/// `edit.select` and `edit.cursor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionsReply {
    pub buffer: BufferId,
    pub rev: u64,
    pub origin: String,
    pub origin_downgraded: bool,
    pub selections: Vec<SelectionW>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorW {
    pub name: String,
    pub start: Point,
    pub end: Option<Point>,
    pub bias: Bias,
    pub collapsed_rev: Option<u64>,
}

/// `edit.anchor.set` and `edit.anchor.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorsReply {
    pub buffer: BufferId,
    pub rev: u64,
    pub anchors: Vec<AnchorW>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearedReply {
    pub buffer: BufferId,
    pub cleared: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KindW {
    Edit,
    Undo,
    Redo,
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViaW {
    pub from: Option<String>,
    pub broker_origin: String,
    pub broker_peer: Option<String>,
    pub broker_service: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntryW {
    pub rev: u64,
    pub origin: String,
    pub lane: String,
    pub kind: KindW,
    pub of: Option<[u64; 2]>,
    pub op_id: Option<String>,
    /// RFC 3339, milliseconds, UTC.
    pub time: String,
    pub via: ViaW,
    /// `null` when elided (`edits_elided`).
    pub edits: Option<Vec<Edit>>,
    pub edits_elided: bool,
    pub text_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryReply {
    pub buffer: BufferId,
    pub rev: u64,
    pub oldest_rev: u64,
    pub entries: Vec<HistoryEntryW>,
    pub truncated: bool,
    pub next: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropsWatchReply {
    pub topic: String,
    pub domain_topics: Vec<String>,
    pub event_seq: u64,
    pub event_sequence: String,
    pub loss_signal: String,
    pub bootstrap: String,
}

/// Every refusal body: `{"error_code","message","reason","buffer","rev",…context}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub error_code: ErrorCode,
    pub message: String,
    pub reason: Option<String>,
    pub buffer: Option<BufferId>,
    pub rev: Option<u64>,
    #[serde(flatten)]
    pub context: Map<String, Value>,
}

// ── events (topic `edit.changed`, inner command `edit.changed`) ─────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub rev: u64,
    pub base_rev: u64,
    pub origin: String,
    pub lane: String,
    pub kind: KindW,
    pub of: Option<[u64; 2]>,
    pub op_id: Option<String>,
    /// Application order; a mirror at `base_rev` applies them in order.
    pub edits: Vec<Edit>,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetSelection {
    pub anchor: usize,
    pub head: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub rev: u64,
    pub origin: String,
    pub selections: Vec<OffsetSelection>,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub rev: u64,
    pub name: String,
    /// `null` start = cleared.
    pub start: Option<usize>,
    pub end: Option<usize>,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub rev: u64,
    pub disk: DiskState,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub path: Option<String>,
    pub rev: u64,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseEvent {
    pub epoch: String,
    pub buffer: BufferId,
    pub event_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    Oversized,
    PublisherLoss,
    Reconnect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResyncTarget {
    All(AllTag),
    Buffers(Vec<BufferId>),
}

/// Refetch (`edit.get snapshot:true`) every named buffer, or all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncEvent {
    pub epoch: String,
    pub buffers: ResyncTarget,
    pub reason: ResyncReason,
    /// New rev for a single-buffer `oversized` resync, else `null`.
    pub rev: Option<u64>,
    pub event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Edit(EditEvent),
    Cursor(CursorEvent),
    Anchor(AnchorEvent),
    Disk(DiskEvent),
    Open(OpenEvent),
    Close(CloseEvent),
    Resync(ResyncEvent),
}
