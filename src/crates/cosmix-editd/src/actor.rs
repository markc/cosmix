//! Per-buffer actors (plan D10, §3.5).
//!
//! One tokio task per buffer processes that buffer's commands in receive order
//! (the single-authority OT order). Its inbox holds `ACTOR_INBOX` commands; the
//! router `try_send`s and answers RESOURCE_LIMIT `busy` when it is full. File I/O
//! (including the initial load) runs as `spawn_blocking` jobs awaited inside
//! this sequence. Watcher notifications never enter the inbox: they set a
//! "recheck disk" flag and wake a `Notify`, so a burst costs one `stat`.
//!
//! # Contract: op_id dedup (frozen; plan §3.5)
//! Owned here, not in the core (the core only records and echoes `op_id`).
//! - Key = [`DedupKey`] `(caller_key, origin, verb, op_id)`, all fixed BEFORE
//!   any lane selection — a retried `edit.undo origin:"*"` hits the cached
//!   reply even if another lane is now newest.
//! - Only SUCCESSFUL replies are cached; a refused request can be retried with
//!   the same `op_id`.
//! - The cache holds the full encoded compact reply, `DEDUP_ENTRIES` per buffer,
//!   LRU; a hit returns it with `duplicate: true` and applies nothing.
//! - The cache dies with the buffer and the epoch: after a restart a retry gets
//!   NOT_FOUND `epoch_mismatch`, never a double apply.
//! - Two anonymous callers collide only if they claim the same origin AND reuse
//!   an `op_id` ("op_ids must be unique per caller").
//!
//! Byte budget: the actor holds one lease covering its text + retained log
//! text + live snapshots. A text mutation leases `Buffer::apply_cost` (the
//! simulated peak) BEFORE applying and reconciles to the real total after; the
//! lease is returned when the actor ends (drop, including a panic unwind).

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cosmix_edit_core::anchor::{AnchorSpec, Selection};
use cosmix_edit_core::buffer::{Applied, Buffer, Cas, Eol, FileMeta, LaneSel, OpSpec, TxnRequest};
use cosmix_edit_core::error::{ErrorCode, reason};
use cosmix_edit_core::history::EntryKind;
use cosmix_edit_core::limits::{FIND_DEFAULT_LIMIT, FIND_MAX_LIMIT, HISTORY_ENTRY_TEXT_MAX, REPLY_CHANGED_MAX};
use cosmix_edit_core::origin::{Origin, Via};
use cosmix_edit_core::pos::{NamedPos, Point, PosSpec, RangeSpec, SelSpec};
use cosmix_edit_core::search::FindQuery;
use cosmix_edit_core::wire::*;
use tokio::sync::{mpsc, oneshot};

use crate::caller::{Caller, CallerKey};
use crate::events::Publisher;
use crate::files::{self, DiskIdentity, Expect, Stat};
use crate::limits::{DEDUP_ENTRIES, MAX_REPLY_BYTES, MAX_SNAPSHOTS_PER_BUFFER};
use crate::props::BufferProps;
use crate::refusal::{RefusalExt, bad_args, from_core, refusal, render};
use crate::router::{Budget, Reply, ToRouter};
use crate::watch::DiskSignal;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey {
    pub caller: CallerKey,
    /// The resolved claimed origin (`kind:label`), not the undo lane.
    pub origin: String,
    pub verb: String,
    pub op_id: String,
}

/// LRU of successful encoded replies. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct DedupCache {
    pub entries: std::collections::VecDeque<(DedupKey, String)>,
}

impl DedupCache {
    /// The cached reply body for `key`, if any.
    pub fn get(&self, key: &DedupKey) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// A hit, refreshed to most-recently-used.
    pub fn hit(&mut self, key: &DedupKey) -> Option<String> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(i)?;
        let body = entry.1.clone();
        self.entries.push_back(entry);
        Some(body)
    }

    /// Record a successful reply, evicting the least recently used.
    pub fn insert(&mut self, key: DedupKey, body: String) {
        self.entries.retain(|(k, _)| k != &key);
        while self.entries.len() >= DEDUP_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, body));
    }
}

/// A buffer-scoped verb, already parsed (argument shape checked).
#[derive(Debug, Clone)]
pub enum BufVerb {
    Save(SaveReq),
    Reload(ReloadReq),
    Get(GetReq),
    Insert(InsertReq),
    Delete(DeleteReq),
    Replace(ReplaceReq),
    Apply(ApplyReq),
    Find(FindReq),
    Select(SelectReq),
    Cursor(CursorReq),
    AnchorSet(AnchorSetReq),
    AnchorGet(AnchorGetReq),
    AnchorClear(AnchorClearReq),
    Undo(UndoReq),
    Redo(UndoReq),
    History(HistoryReq),
}

impl BufVerb {
    pub fn verb(&self) -> &'static str {
        match self {
            BufVerb::Save(_) => "edit.save",
            BufVerb::Reload(_) => "edit.reload",
            BufVerb::Get(_) => "edit.get",
            BufVerb::Insert(_) => "edit.insert",
            BufVerb::Delete(_) => "edit.delete",
            BufVerb::Replace(_) => "edit.replace",
            BufVerb::Apply(_) => "edit.apply",
            BufVerb::Find(_) => "edit.find",
            BufVerb::Select(_) => "edit.select",
            BufVerb::Cursor(_) => "edit.cursor",
            BufVerb::AnchorSet(_) => "edit.anchor.set",
            BufVerb::AnchorGet(_) => "edit.anchor.get",
            BufVerb::AnchorClear(_) => "edit.anchor.clear",
            BufVerb::Undo(_) => "edit.undo",
            BufVerb::Redo(_) => "edit.redo",
            BufVerb::History(_) => "edit.history",
        }
    }

    fn op_id(&self) -> Option<&str> {
        match self {
            BufVerb::Save(r) => r.meta.op_id.as_deref(),
            BufVerb::Reload(r) => r.meta.op_id.as_deref(),
            BufVerb::Insert(r) => r.args.meta.op_id.as_deref(),
            BufVerb::Delete(r) => r.args.meta.op_id.as_deref(),
            BufVerb::Replace(r) => r.args.meta.op_id.as_deref(),
            BufVerb::Apply(r) => r.args.meta.op_id.as_deref(),
            BufVerb::Select(r) => r.meta.op_id.as_deref(),
            BufVerb::Cursor(r) => r.meta.op_id.as_deref(),
            BufVerb::AnchorSet(r) => r.meta.op_id.as_deref(),
            BufVerb::AnchorClear(r) => r.meta.op_id.as_deref(),
            BufVerb::Undo(r) | BufVerb::Redo(r) => r.op_id.as_deref(),
            BufVerb::Get(_) | BufVerb::Find(_) | BufVerb::AnchorGet(_) | BufVerb::History(_) => None,
        }
    }
}

pub enum ActorMsg {
    Cmd { verb: BufVerb, caller: Caller, reply: oneshot::Sender<Reply> },
    /// From the router: the last holder is closing (or `force`).
    Close { force: bool, reply: oneshot::Sender<Reply> },
}

pub enum Init {
    Scratch { language: Option<String> },
    Load { path: PathBuf, opened_as: String, create: bool, language: Option<String> },
}

/// Everything an actor starts with.
pub struct ActorInit {
    pub bid: BufferId,
    pub epoch: String,
    pub init: Init,
    pub budget: Arc<Budget>,
    /// Bytes the router already leased for the load (stat'd size).
    pub leased: u64,
    pub publisher: Arc<Publisher>,
    pub to_router: mpsc::UnboundedSender<ToRouter>,
    pub signal: Arc<DiskSignal>,
    pub snapshot_seq: Arc<AtomicU64>,
    pub rx: mpsc::Receiver<ActorMsg>,
}

/// The actor's byte lease; returned on drop (also on a panic unwind).
struct Lease {
    budget: Arc<Budget>,
    held: u64,
}

impl Lease {
    fn take(&mut self, n: u64) -> bool {
        if self.budget.try_lease(n) {
            self.held += n;
            true
        } else {
            false
        }
    }

    /// Hold exactly `target` bytes (best effort when growing past the cap).
    fn reconcile(&mut self, target: u64) {
        if target < self.held {
            self.budget.release(self.held - target);
            self.held = target;
        } else if target > self.held && self.budget.try_lease(target - self.held) {
            self.held = target;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.budget.release(self.held);
    }
}

struct Snap {
    token: String,
    rev: u64,
    text: Arc<str>,
}

struct Actor {
    bid: BufferId,
    epoch: String,
    buffer: Buffer,
    meta: FileMeta,
    path: Option<PathBuf>,
    opened_as: Option<String>,
    language: String,
    base: Option<DiskIdentity>,
    observed: Option<Stat>,
    disk: DiskState,
    dedup: DedupCache,
    snapshots: VecDeque<Snap>,
    lease: Lease,
    publisher: Arc<Publisher>,
    to_router: mpsc::UnboundedSender<ToRouter>,
    signal: Arc<DiskSignal>,
    snapshot_seq: Arc<AtomicU64>,
    origin_last: Option<String>,
    pushed: Option<BufferProps>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Run CPU-heavy core work without starving the other actors on this worker.
fn heavy<R>(f: impl FnOnce() -> R) -> R {
    let multi = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi { tokio::task::block_in_place(f) } else { f() }
}

async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R, Refusal> {
    tokio::task::spawn_blocking(f).await.map_err(|e| crate::refusal::internal(format!("blocking job failed: {e}")))
}

/// JSON-escaped length of one char (serde_json's escaping).
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Bytes of fixed JSON around a `get` page (fields other than the text).
const GET_OVERHEAD: usize = 4096;
/// Encoded overhead of one numbered line object.
const NUMBERED_LINE_OVERHEAD: usize = 48;
/// Reply bytes kept back from `find`'s match budget for the envelope.
const FIND_OVERHEAD: usize = 64 * 1024;
/// Reply bytes kept back from `history`'s budget.
const HISTORY_OVERHEAD: usize = 4096;

fn kind_w(kind: &EntryKind) -> (KindW, Option<[u64; 2]>) {
    match kind {
        EntryKind::Edit => (KindW::Edit, None),
        EntryKind::Undo { of } => (KindW::Undo, Some([*of.start(), *of.end()])),
        EntryKind::Redo { of } => (KindW::Redo, Some([*of.start(), *of.end()])),
        EntryKind::Reload => (KindW::Reload, None),
    }
}

fn via_w(via: &Via) -> ViaW {
    ViaW {
        from: via.from.clone(),
        broker_origin: via.broker_origin.clone(),
        broker_peer: via.broker_peer.clone(),
        broker_service: via.broker_service.clone(),
    }
}

fn rfc3339_ms(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn internal_via() -> Via {
    Via { from: None, broker_origin: "internal".into(), broker_peer: None, broker_service: None }
}

/// A text source for paged reads: the live buffer or a frozen snapshot.
enum Src<'a> {
    Live(&'a Buffer),
    Snap(&'a str),
}

impl Src<'_> {
    fn len(&self) -> usize {
        match self {
            Src::Live(b) => b.len(),
            Src::Snap(s) => s.len(),
        }
    }

    fn is_boundary(&self, off: usize) -> bool {
        match self {
            Src::Live(b) => b.resolve_pos(&PosSpec::Offset(off)).is_ok(),
            Src::Snap(s) => s.is_char_boundary(off),
        }
    }

    fn floor(&self, mut off: usize) -> usize {
        while off > 0 && !self.is_boundary(off) {
            off -= 1;
        }
        off
    }

    fn read(&self, r: std::ops::Range<usize>) -> String {
        match self {
            Src::Live(b) => {
                let mut out = String::with_capacity(r.len());
                b.read(r, &mut out);
                out
            }
            Src::Snap(s) => s[r].to_string(),
        }
    }

    fn point(&self, off: usize) -> Point {
        match self {
            Src::Live(b) => b.point(off),
            Src::Snap(s) => str_point(s, off),
        }
    }
}

fn str_point(s: &str, off: usize) -> Point {
    let before = &s[..off];
    let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    Point { offset: off, line, col: s[line_start..off].chars().count() + 1 }
}

fn pos_refusal(reason_: &'static str, message: String) -> Refusal {
    refusal(ErrorCode::InvalidArgument, Some(reason_), message)
}

fn str_line_start(s: &str, line: usize) -> Result<usize, Refusal> {
    let total = s.bytes().filter(|&b| b == b'\n').count() + 1;
    if line == 0 || line > total {
        return Err(pos_refusal(reason::LINE_OUT_OF_RANGE, format!("line {line} is outside 1..={total}"))
            .with("line", line));
    }
    if line == 1 {
        return Ok(0);
    }
    Ok(s.match_indices('\n').nth(line - 2).map(|(i, _)| i + 1).unwrap_or(s.len()))
}

fn str_pos(s: &str, p: &PosSpec) -> Result<usize, Refusal> {
    match p {
        PosSpec::Offset(o) => {
            if *o > s.len() {
                Err(pos_refusal(reason::OFFSET_OUT_OF_RANGE, format!("offset {o} is past the end ({})", s.len())))
            } else if !s.is_char_boundary(*o) {
                Err(pos_refusal(reason::NOT_CHAR_BOUNDARY, format!("offset {o} is inside a UTF-8 sequence")))
            } else {
                Ok(*o)
            }
        }
        PosSpec::Named(NamedPos::Start) => Ok(0),
        PosSpec::Named(NamedPos::End) => Ok(s.len()),
        PosSpec::LineCol { line, col } => {
            let start = str_line_start(s, *line)?;
            let end = s[start..].find('\n').map(|i| start + i).unwrap_or(s.len());
            let col = col.unwrap_or(1);
            if col == 0 {
                return Err(pos_refusal(reason::COL_OUT_OF_RANGE, format!("col {col} is out of range")));
            }
            let mut offs = s[start..end].char_indices().map(|(i, _)| start + i).chain(std::iter::once(end));
            offs.nth(col - 1)
                .ok_or_else(|| pos_refusal(reason::COL_OUT_OF_RANGE, format!("col {col} is past the end of line {line}")))
        }
        PosSpec::Anchor { .. } => Err(bad_args("snapshot pages resolve offsets, lines and line/col; not anchors")),
    }
}

fn str_range(s: &str, r: &RangeSpec) -> Result<std::ops::Range<usize>, Refusal> {
    let (a, b) = match r {
        RangeSpec::Offsets([a, b]) => (str_pos(s, &PosSpec::Offset(*a))?, str_pos(s, &PosSpec::Offset(*b))?),
        RangeSpec::Span { start, end } => (str_pos(s, start)?, str_pos(s, end)?),
        RangeSpec::Lines { lines: [a, b] } => {
            if a > b {
                return Err(pos_refusal(reason::LINE_OUT_OF_RANGE, format!("lines [{a}, {b}] are reversed")));
            }
            let start = str_line_start(s, *a)?;
            str_line_start(s, *b)?;
            let end = str_line_start(s, b + 1).unwrap_or(s.len());
            (start, end)
        }
        RangeSpec::All(_) => (0, s.len()),
        RangeSpec::Anchor { .. } => {
            return Err(bad_args("snapshot pages resolve offsets, lines and line/col; not anchors"));
        }
    };
    if a > b {
        return Err(bad_args(format!("range [{a}, {b}) is reversed")));
    }
    Ok(a..b)
}

/// One `get` page over `src` (plan §4.4): encoded-budgeted, always progressing.
fn page(src: &Src, range: std::ops::Range<usize>, numbered: bool) -> (Option<String>, Option<Vec<NumberedLine>>, Point, Point, bool) {
    let budget = MAX_REPLY_BYTES - GET_OVERHEAD;
    let start = range.start;
    let raw_end = src.floor(range.end.min(start.saturating_add(budget)));
    let text = src.read(start..raw_end.max(start));
    let start_point = src.point(start);
    let mut used = 0usize;
    let mut cut = 0usize;
    let mut first = true;
    let mut line_start = true;
    for (i, c) in text.char_indices() {
        let mut cost = escaped_len(c);
        if numbered && line_start {
            cost += NUMBERED_LINE_OVERHEAD;
        }
        if used + cost > budget && !first {
            break;
        }
        used += cost;
        cut = i + c.len_utf8();
        first = false;
        line_start = c == '\n';
    }
    let end = start + cut;
    let truncated = end < range.end;
    let end_point = src.point(end);
    let text = &text[..cut];
    if !numbered {
        return (Some(text.to_string()), None, start_point, end_point, truncated);
    }
    let mut lines = Vec::new();
    let segments: Vec<&str> = text.split('\n').collect();
    let at_buffer_end = end == src.len();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        // The piece after a final '\n' is a real (empty) line only at the buffer end.
        if last && seg.is_empty() && i > 0 && !at_buffer_end {
            break;
        }
        if last && seg.is_empty() && i == 0 && cut > 0 {
            break;
        }
        let terminated = !last;
        let seg = if terminated { seg.strip_suffix('\r').unwrap_or(seg) } else { seg };
        lines.push(NumberedLine {
            line: start_point.line + i,
            text: seg.to_string(),
            cont: i == 0 && start_point.col != 1,
        });
    }
    (None, Some(lines), start_point, end_point, truncated)
}

impl Actor {
    fn dirty(&self) -> bool {
        // A never-saved buffer at rev 0 is empty and unchanged: not dirty.
        self.buffer.is_dirty() && !(self.buffer.saved_rev().is_none() && self.buffer.rev() == 0)
    }

    fn name(&self) -> Option<String> {
        self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
    }

    fn props(&self) -> BufferProps {
        BufferProps {
            path: self.path.as_ref().map(|p| p.display().to_string()),
            opened_as: self.opened_as.clone(),
            name: self.name(),
            language: self.language.clone(),
            eol: self.meta.eol,
            bom: self.meta.bom,
            dirty: self.dirty(),
            saved_rev: self.buffer.saved_rev(),
            disk: self.disk,
            rev: self.buffer.rev(),
            lines: self.buffer.line_count(),
            bytes: self.buffer.len(),
            origin_last: self.origin_last.clone(),
        }
    }

    /// Push coarse state to the router when it changed.
    fn push_state(&mut self) {
        let props = self.props();
        if self.pushed.as_ref() != Some(&props) {
            self.pushed = Some(props.clone());
            let _ = self.to_router.send(ToRouter::State { bid: self.bid.clone(), props });
        }
    }

    fn footprint(&self) -> u64 {
        let snaps: usize = self.snapshots.iter().map(|s| s.text.len()).sum();
        (self.buffer.len() + self.buffer.log_text_bytes() + snaps) as u64
    }

    fn reconcile(&mut self) {
        let target = self.footprint();
        self.lease.reconcile(target);
    }

    fn with_ctx(&self, r: Refusal) -> Refusal {
        let mut r = r;
        if r.buffer.is_none() {
            r.buffer = Some(self.bid.clone());
        }
        if r.rev.is_none() && matches!(r.error_code, ErrorCode::Conflict) {
            r.rev = Some(self.buffer.rev());
        }
        r
    }

    fn core(&self, e: cosmix_edit_core::error::CoreError) -> Refusal {
        let mut r = from_core(e, Some(&self.bid));
        if r.rev.is_none() && matches!(r.error_code, ErrorCode::Conflict) {
            r.rev = Some(self.buffer.rev());
        }
        r
    }

    fn stale(&self, expect: Option<u64>) -> Result<(), Refusal> {
        match expect {
            Some(r) if r != self.buffer.rev() => Err(refusal(
                ErrorCode::Conflict,
                Some(reason::STALE_REV),
                format!("expect_rev {r} is stale: buffer {} is at rev {}", self.bid, self.buffer.rev()),
            )
            .buffer(&self.bid)
            .rev(self.buffer.rev())),
            _ => Ok(()),
        }
    }

    fn span(&self, r: &std::ops::Range<usize>) -> Span {
        Span { start: self.buffer.point(r.start), end: self.buffer.point(r.end) }
    }

    /// The newest log entry (the one `applied` just recorded).
    fn entry_lane_origin(&self, rev: u64) -> (String, String) {
        self.buffer
            .history(rev.saturating_sub(1), 1)
            .find(|e| e.rev == rev)
            .map(|e| (e.origin.to_string(), e.lane.to_string()))
            .unwrap_or_default()
    }

    fn mutation_reply(&self, applied: &Applied, origin: &Origin, downgraded: bool) -> MutationReply {
        let changed: Vec<Span> = applied.changed.iter().take(REPLY_CHANGED_MAX).map(|r| self.span(r)).collect();
        let envelope = match (applied.changed.iter().map(|r| r.start).min(), applied.changed.iter().map(|r| r.end).max()) {
            (Some(s), Some(e)) => Some(self.span(&(s..e))),
            _ => None,
        };
        let cursor = self
            .buffer
            .selections()
            .find(|(o, _)| *o == origin)
            .and_then(|(_, sels)| sels.first().copied())
            .map(|s| self.buffer.point(s.head))
            .or(envelope.map(|s| s.end))
            .or(applied.edits.last().map(|e| self.buffer.point(e.offset.min(self.buffer.len()))));
        MutationReply {
            buffer: self.bid.clone(),
            epoch: self.epoch.clone(),
            rev: applied.rev,
            base_rev: applied.base_rev,
            origin: origin.to_string(),
            origin_downgraded: downgraded,
            op_id: applied.op_id.clone(),
            duplicate: false,
            rebased: applied.rebased,
            edit_count: applied.edits.len(),
            inserted_bytes: applied.inserted_bytes,
            deleted_bytes: applied.deleted_bytes,
            changed_span: envelope,
            changed_truncated: applied.changed.len() > REPLY_CHANGED_MAX,
            changed,
            cursor,
            dirty: self.dirty(),
            lines: self.buffer.line_count(),
            bytes: self.buffer.len(),
            history_trimmed_to: applied.history_trimmed_to,
        }
    }

    /// Publish the `edit` event for `applied` (or `resync oversized` when its
    /// inserted text alone would exceed the event budget — never copied).
    fn publish_edit(&mut self, applied: &Applied) {
        let (origin, lane) = self.entry_lane_origin(applied.rev);
        self.origin_last = Some(origin.clone());
        let (kind, of) = kind_w(&applied.kind);
        let inserted: usize = applied.edits.iter().map(|e| e.insert.len()).sum();
        if inserted > crate::limits::MAX_EVENT_BYTES {
            // Certainly oversized: announce without copying the text.
            self.publisher.oversized(&self.bid, applied.rev);
            return;
        }
        let edits = applied.edits.clone();
        self.publisher.event(
            Some(&self.bid),
            Event::Edit(EditEvent {
                epoch: self.epoch.clone(),
                buffer: self.bid.clone(),
                rev: applied.rev,
                base_rev: applied.base_rev,
                origin,
                lane,
                kind,
                of,
                op_id: applied.op_id.clone(),
                edits,
                event_seq: 0,
            }),
        );
    }

    fn publish_disk(&self) {
        self.publisher.event(
            Some(&self.bid),
            Event::Disk(DiskEvent {
                epoch: self.epoch.clone(),
                buffer: self.bid.clone(),
                rev: self.buffer.rev(),
                disk: self.disk,
                event_seq: 0,
            }),
        );
    }

    fn selections_of(&self, origin: &Origin) -> Vec<Selection> {
        self.buffer.selections().find(|(o, _)| *o == origin).map(|(_, s)| s.to_vec()).unwrap_or_default()
    }

    fn publish_cursor(&self, origin: &Origin) {
        let selections =
            self.selections_of(origin).iter().map(|s| OffsetSelection { anchor: s.anchor, head: s.head }).collect();
        self.publisher.event(
            Some(&self.bid),
            Event::Cursor(CursorEvent {
                epoch: self.epoch.clone(),
                buffer: self.bid.clone(),
                rev: self.buffer.rev(),
                origin: origin.to_string(),
                selections,
                event_seq: 0,
            }),
        );
    }

    fn anchor_w(&self, name: &str) -> Option<AnchorW> {
        let a = self.buffer.anchor(name)?;
        Some(AnchorW {
            name: a.name.clone(),
            start: self.buffer.point(a.start.offset),
            end: a.end.map(|e| self.buffer.point(e.offset)),
            bias: a.start.bias,
            collapsed_rev: a.start.collapsed_rev.or(a.end.and_then(|e| e.collapsed_rev)),
        })
    }

    // ── verbs ───────────────────────────────────────────────────────────────

    async fn handle(&mut self, verb: BufVerb, caller: &Caller) -> Result<String, Refusal> {
        match verb {
            BufVerb::Insert(r) => {
                let ops = vec![OpSpec::Insert { at: r.at, text: r.text }];
                self.text_mutation(ops, r.args, caller)
            }
            BufVerb::Delete(r) => self.text_mutation(vec![OpSpec::Delete { range: r.range }], r.args, caller),
            BufVerb::Replace(r) => {
                self.text_mutation(vec![OpSpec::Replace { range: r.range, text: r.text }], r.args, caller)
            }
            BufVerb::Apply(r) => self.text_mutation(r.ops, r.args, caller),
            BufVerb::Undo(r) => self.undo_redo(r, caller, false),
            BufVerb::Redo(r) => self.undo_redo(r, caller, true),
            BufVerb::Get(r) => self.get(r),
            BufVerb::Find(r) => self.find(r),
            BufVerb::History(r) => self.history(r),
            BufVerb::Select(r) => self.select(r.ranges, caller),
            BufVerb::Cursor(r) => self.select(vec![SelSpec::Range(RangeSpec::Span { start: r.at.clone(), end: r.at })], caller),
            BufVerb::AnchorSet(r) => self.anchor_set(r),
            BufVerb::AnchorGet(r) => self.anchor_get(r),
            BufVerb::AnchorClear(r) => {
                let cleared = self.buffer.anchor_clear(&r.name);
                if cleared {
                    self.publisher.event(
                        Some(&self.bid),
                        Event::Anchor(AnchorEvent {
                            epoch: self.epoch.clone(),
                            buffer: self.bid.clone(),
                            rev: self.buffer.rev(),
                            name: r.name,
                            start: None,
                            end: None,
                            event_seq: 0,
                        }),
                    );
                }
                Ok(json(&ClearedReply { buffer: self.bid.clone(), cleared }))
            }
            BufVerb::Save(r) => self.save(r, caller).await,
            BufVerb::Reload(r) => self.reload(r, caller).await,
        }
    }

    fn text_mutation(&mut self, ops: Vec<OpSpec>, args: TextMutArgs, caller: &Caller) -> Result<String, Refusal> {
        let cas = match (args.cas.expect_rev, args.cas.base_rev) {
            (Some(r), None) => Cas::ExpectRev(r),
            (None, Some(b)) => Cas::BaseRev(b),
            _ => Cas::Latest,
        };
        let req = TxnRequest { ops, cas, coalesce: args.coalesce, cursor: args.cursor, op_id: args.meta.op_id };
        let cost = self.buffer.apply_cost(&req).map_err(|e| self.core(e))? as u64;
        if !self.lease.take(cost) {
            return Err(crate::refusal::budget(cost).buffer(&self.bid).rev(self.buffer.rev()));
        }
        let applied = heavy(|| self.buffer.apply(req, &caller.origin, caller.via.clone(), now_ms()));
        let applied = match applied {
            Ok(a) => a,
            Err(e) => {
                self.reconcile();
                return Err(self.core(e));
            }
        };
        self.reconcile();
        self.publish_edit(&applied);
        self.push_state();
        Ok(json(&self.mutation_reply(&applied, &caller.origin, caller.origin_downgraded)))
    }

    fn undo_redo(&mut self, r: UndoReq, caller: &Caller, redo: bool) -> Result<String, Refusal> {
        self.stale(r.expect_rev)?;
        let sel = match r.origin.as_deref() {
            None => LaneSel::Own,
            Some("*") => LaneSel::All,
            Some(lane) => LaneSel::Lane(lane.parse::<Origin>().map_err(|e| self.core(e))?),
        };
        let result = heavy(|| {
            if redo {
                self.buffer.redo(sel, &caller.origin, caller.via.clone(), now_ms())
            } else {
                self.buffer.undo(sel, &caller.origin, caller.via.clone(), now_ms())
            }
        });
        self.reconcile();
        let applied = result.map_err(|e| self.core(e))?;
        self.publish_edit(&applied);
        self.push_state();
        let (_, lane) = self.entry_lane_origin(applied.rev);
        let (_, of) = kind_w(&applied.kind);
        let reply = UndoReply {
            mutation: self.mutation_reply(&applied, &caller.origin, caller.origin_downgraded),
            lane,
            undid: if redo { None } else { of },
            redid: if redo { of } else { None },
        };
        Ok(json(&reply))
    }

    fn get(&mut self, r: GetReq) -> Result<String, Refusal> {
        let (snap_index, token) = match &r.snapshot {
            Some(SnapshotArg::Token(t)) => {
                let i = self.snapshots.iter().position(|s| &s.token == t).ok_or_else(|| {
                    refusal(
                        ErrorCode::NotFound,
                        Some(reason::SNAPSHOT_EXPIRED),
                        format!("snapshot {t} was released; start again with snapshot:true"),
                    )
                    .buffer(&self.bid)
                    .rev(self.buffer.rev())
                })?;
                (Some(i), Some(t.clone()))
            }
            Some(SnapshotArg::Start(true)) => {
                self.stale(r.expect_rev)?;
                let size = self.buffer.len() as u64;
                while self.snapshots.len() >= MAX_SNAPSHOTS_PER_BUFFER {
                    self.snapshots.pop_front();
                    self.reconcile();
                }
                if !self.lease.take(size) {
                    return Err(crate::refusal::budget(size).buffer(&self.bid).rev(self.buffer.rev()));
                }
                let text = self.buffer.snapshot();
                let token = format!("s{}", self.snapshot_seq.fetch_add(1, Ordering::AcqRel) + 1);
                self.snapshots.push_back(Snap { token: token.clone(), rev: self.buffer.rev(), text });
                self.reconcile();
                (Some(self.snapshots.len() - 1), Some(token))
            }
            _ => {
                self.stale(r.expect_rev)?;
                (None, None)
            }
        };
        let spec = r.range.clone().unwrap_or(RangeSpec::All(AllTag::All));
        let (rev, text, lines, start, end, truncated, bytes_total, lines_total) = {
            let (src, rev, totals) = match snap_index {
                Some(i) => {
                    let s = &self.snapshots[i].text;
                    let totals = (s.len(), s.bytes().filter(|&b| b == b'\n').count() + 1);
                    (Src::Snap(s), self.snapshots[i].rev, totals)
                }
                None => (Src::Live(&self.buffer), self.buffer.rev(), (self.buffer.len(), self.buffer.line_count())),
            };
            let range = match &src {
                Src::Live(b) => b.resolve_range(&spec).map_err(|e| self.core(e))?,
                Src::Snap(s) => str_range(s, &spec).map_err(|e| self.with_ctx(e))?,
            };
            let (text, lines, start, end, truncated) = heavy(|| page(&src, range, r.numbered));
            (rev, text, lines, start, end, truncated, totals.0, totals.1)
        };
        let next = truncated.then_some(end.offset);
        if let (Some(i), false) = (snap_index, truncated) {
            // The final page of a snapshot read releases it.
            self.snapshots.remove(i);
            self.reconcile();
        }
        Ok(json(&GetReply {
            buffer: self.bid.clone(),
            epoch: self.epoch.clone(),
            rev,
            text,
            lines,
            start,
            end,
            bytes_total,
            lines_total,
            truncated,
            next,
            snapshot: token,
        }))
    }

    fn find(&mut self, r: FindReq) -> Result<String, Refusal> {
        let q = FindQuery {
            pattern: r.pattern,
            regex: r.regex,
            case: r.case,
            range: r.range,
            groups: r.groups,
            limit: r.limit.unwrap_or(FIND_DEFAULT_LIMIT).clamp(1, FIND_MAX_LIMIT),
            from: r.from,
        };
        let found = heavy(|| self.buffer.find(&q, MAX_REPLY_BYTES - FIND_OVERHEAD)).map_err(|e| self.core(e))?;
        let matches = found
            .matches
            .into_iter()
            .map(|m| MatchW {
                start: self.buffer.point(m.range.start),
                end: self.buffer.point(m.range.end),
                text: m.text,
                text_truncated: m.text_truncated,
                groups: m.groups,
                groups_truncated: m.groups_truncated,
            })
            .collect();
        Ok(json(&FindReply {
            buffer: self.bid.clone(),
            rev: self.buffer.rev(),
            matches,
            truncated: found.truncated,
            next: found.next,
        }))
    }

    fn history(&self, r: HistoryReq) -> Result<String, Refusal> {
        let limit = r.limit.unwrap_or(100).clamp(1, 10_000);
        let budget = MAX_REPLY_BYTES - HISTORY_OVERHEAD;
        let mut used = 0usize;
        let mut entries = Vec::new();
        let mut last = None;
        for e in self.buffer.history(r.since_rev, limit) {
            let text_bytes = e.edits.iter().map(|x| x.insert.len()).sum::<usize>()
                + e.deleted.iter().map(String::len).sum::<usize>();
            let elide = text_bytes > HISTORY_ENTRY_TEXT_MAX;
            let (kind, of) = kind_w(&e.kind);
            let w = HistoryEntryW {
                rev: e.rev,
                origin: e.origin.to_string(),
                lane: e.lane.to_string(),
                kind,
                of,
                op_id: e.op_id.clone(),
                time: rfc3339_ms(e.time_ms),
                via: via_w(&e.via),
                edits: if elide { None } else { Some(e.edits.clone()) },
                edits_elided: elide,
                text_bytes,
            };
            let size = crate::events::encoded_len(&w) + 1;
            if used + size > budget && !entries.is_empty() {
                break;
            }
            used += size;
            last = Some(e.rev);
            entries.push(w);
        }
        let more = last.is_some_and(|l| self.buffer.history(l, 1).next().is_some());
        Ok(json(&HistoryReply {
            buffer: self.bid.clone(),
            rev: self.buffer.rev(),
            oldest_rev: self.buffer.oldest_rev(),
            entries,
            truncated: more,
            next: if more { last } else { None },
        }))
    }

    fn select(&mut self, specs: Vec<SelSpec>, caller: &Caller) -> Result<String, Refusal> {
        let mut sels = Vec::with_capacity(specs.len());
        for spec in &specs {
            let sel = match spec {
                SelSpec::Directed { anchor, head } => Selection {
                    anchor: self.buffer.resolve_pos(anchor).map_err(|e| self.core(e))?,
                    head: self.buffer.resolve_pos(head).map_err(|e| self.core(e))?,
                },
                SelSpec::Range(r) => {
                    let r = self.buffer.resolve_range(r).map_err(|e| self.core(e))?;
                    Selection { anchor: r.start, head: r.end }
                }
            };
            sels.push(sel);
        }
        self.buffer.set_selections(&caller.origin, sels).map_err(|e| self.core(e))?;
        self.publish_cursor(&caller.origin);
        let selections = self
            .selections_of(&caller.origin)
            .iter()
            .map(|s| SelectionW { anchor: self.buffer.point(s.anchor), head: self.buffer.point(s.head) })
            .collect();
        Ok(json(&SelectionsReply {
            buffer: self.bid.clone(),
            rev: self.buffer.rev(),
            origin: caller.origin.to_string(),
            origin_downgraded: caller.origin_downgraded,
            selections,
        }))
    }

    fn anchor_set(&mut self, r: AnchorSetReq) -> Result<String, Refusal> {
        let spec = AnchorSpec { at: r.at, range: r.range, bias: r.bias };
        self.buffer.anchor_set(&r.name, spec).map_err(|e| self.core(e))?;
        let w = self.anchor_w(&r.name);
        if let Some(w) = &w {
            self.publisher.event(
                Some(&self.bid),
                Event::Anchor(AnchorEvent {
                    epoch: self.epoch.clone(),
                    buffer: self.bid.clone(),
                    rev: self.buffer.rev(),
                    name: w.name.clone(),
                    start: Some(w.start.offset),
                    end: w.end.map(|e| e.offset),
                    event_seq: 0,
                }),
            );
        }
        Ok(json(&AnchorsReply { buffer: self.bid.clone(), rev: self.buffer.rev(), anchors: w.into_iter().collect() }))
    }

    fn anchor_get(&self, r: AnchorGetReq) -> Result<String, Refusal> {
        let anchors = match &r.name {
            Some(name) => vec![self.anchor_w(name).ok_or_else(|| {
                refusal(ErrorCode::NotFound, Some(reason::UNKNOWN_ANCHOR), format!("no anchor {name}"))
                    .buffer(&self.bid)
                    .rev(self.buffer.rev())
            })?],
            None => self.buffer.anchors().filter_map(|a| self.anchor_w(&a.name)).collect(),
        };
        Ok(json(&AnchorsReply { buffer: self.bid.clone(), rev: self.buffer.rev(), anchors }))
    }

    async fn save(&mut self, r: SaveReq, _caller: &Caller) -> Result<String, Refusal> {
        let target = match &r.path {
            Some(p) => {
                let p = p.clone();
                let resolved = blocking(move || files::resolve_path(&p)).await?.map_err(|e| self.with_ctx(e))?;
                Some(resolved)
            }
            None => None,
        };
        let save_as = match (&target, &self.path) {
            (Some(t), Some(own)) if t == own => None,
            (Some(t), _) => Some(t.clone()),
            (None, Some(_)) => None,
            (None, None) => {
                return Err(refusal(
                    ErrorCode::InvalidArgument,
                    Some(reason::SCRATCH_NEEDS_PATH),
                    "a scratch buffer needs path to save",
                )
                .buffer(&self.bid)
                .rev(self.buffer.rev()));
            }
        };
        self.stale(r.expect_rev)?;
        let bytes = self.buffer.to_bytes(&self.meta);
        let saved = match &save_as {
            None => {
                let dest = self.path.clone().unwrap_or_default();
                let expect = self.base.map(Expect::Identity).unwrap_or(Expect::Absent);
                let force = r.force;
                blocking(move || files::save(&dest, &bytes, expect, force)).await?.map_err(|e| self.with_ctx(e))?
            }
            Some(dest) => {
                let (tx, rx) = oneshot::channel();
                let _ = self.to_router.send(ToRouter::ReserveSaveAs { bid: self.bid.clone(), path: dest.clone(), reply: tx });
                rx.await
                    .map_err(|_| crate::refusal::internal("router gone"))?
                    .map_err(|e| self.with_ctx(e).rev(self.buffer.rev()))?;
                let release = |me: &Self| {
                    let _ = me.to_router.send(ToRouter::ReleaseSaveAs { path: dest.clone() });
                };
                let probe = dest.clone();
                let observed = match blocking(move || files::identity(&probe)).await? {
                    Ok(o) => o,
                    Err(e) => {
                        release(self);
                        return Err(self.with_ctx(crate::refusal::io_error(&format!("reading {}", dest.display()), &e)));
                    }
                };
                let expect = match observed {
                    None => Expect::Absent,
                    Some(_) if !r.force => {
                        release(self);
                        let shown = dest.display().to_string();
                        return Err(refusal(
                            ErrorCode::Conflict,
                            Some(reason::EXISTS),
                            format!("{shown} exists; pass force:true to overwrite it"),
                        )
                        .with("path", shown)
                        .buffer(&self.bid)
                        .rev(self.buffer.rev()));
                    }
                    Some(id) => Expect::Identity(id),
                };
                let d = dest.clone();
                match blocking(move || files::save(&d, &bytes, expect, false)).await? {
                    Ok(saved) => {
                        let (tx, rx) = oneshot::channel();
                        let _ = self.to_router.send(ToRouter::CommitSaveAs {
                            bid: self.bid.clone(),
                            old: self.path.clone(),
                            new: dest.clone(),
                            reply: tx,
                        });
                        let _ = rx.await;
                        self.path = Some(dest.clone());
                        self.opened_as = r.path.clone();
                        saved
                    }
                    Err(e) => {
                        release(self);
                        return Err(self.with_ctx(e));
                    }
                }
            }
        };
        self.base = Some(saved.base);
        self.observed = Some(saved.base.stat());
        self.buffer.mark_saved();
        self.disk = if self.signal.is_unwatched() { DiskState::Unwatched } else { DiskState::Clean };
        self.push_state();
        Ok(json(&SaveReply {
            buffer: self.bid.clone(),
            epoch: self.epoch.clone(),
            path: self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            rev: self.buffer.rev(),
            saved_rev: self.buffer.rev(),
            file_bytes: saved.file_bytes,
            disk: DiskState::Clean,
            durable: saved.durable,
            warning: saved.warning,
        }))
    }

    /// Read the bound file and decode it (BOM, UTF-8, limits) off the runtime.
    /// Borrows nothing across the await (a `Buffer` is `Send`, not `Sync`).
    fn read_disk(&self) -> impl Future<Output = Result<(DiskIdentity, String, FileMeta), Refusal>> + Send + 'static {
        let path = self.path.clone();
        let bid = self.bid.clone();
        async move { read_disk(path, bid).await }
    }
}

async fn read_disk(path: Option<PathBuf>, bid: BufferId) -> Result<(DiskIdentity, String, FileMeta), Refusal> {
    {
        let Some(path) = path else {
            return Err(refusal(ErrorCode::InvalidArgument, Some(reason::SCRATCH_NEEDS_PATH), "a scratch buffer has no file")
                .buffer(&bid));
        };
        let shown = path.display().to_string();
        let read = blocking(move || -> Result<(DiskIdentity, String, FileMeta), Refusal> {
            let (stat, bytes) = match files::read_bounded(&path) {
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(files::file_not_found(&shown)),
                Err(e) => return Err(crate::refusal::io_error(&format!("reading {shown}"), &e)),
            };
            let id = DiskIdentity::from_parts(stat, &bytes);
            let (tmp, meta) = Buffer::from_bytes(&bytes).map_err(|e| from_core(e, None).with("path", shown.clone()))?;
            let mut text = String::with_capacity(tmp.len());
            tmp.read(0..tmp.len(), &mut text);
            Ok((id, text, meta))
        })
        .await?;
        read.map_err(|mut e| {
            e.buffer.get_or_insert(bid);
            e
        })
    }
}

impl Actor {
    async fn reload(&mut self, r: ReloadReq, caller: &Caller) -> Result<String, Refusal> {
        if self.path.is_none() {
            return Err(refusal(
                ErrorCode::InvalidArgument,
                Some(reason::SCRATCH_NEEDS_PATH),
                "a scratch buffer has no file to reload",
            )
            .buffer(&self.bid)
            .rev(self.buffer.rev()));
        }
        self.stale(r.expect_rev)?;
        if self.dirty() && !r.force {
            return Err(refusal(
                ErrorCode::Conflict,
                Some(reason::DIRTY),
                format!("buffer {} has unsaved changes; reload with force:true to discard them", self.bid),
            )
            .buffer(&self.bid)
            .rev(self.buffer.rev()));
        }
        let (id, text, meta) = self.read_disk().await?;
        let peak = (text.len() + self.buffer.len()) as u64;
        if !self.lease.take(peak) {
            return Err(crate::refusal::budget(peak).buffer(&self.bid).rev(self.buffer.rev()));
        }
        let applied = heavy(|| self.buffer.reload_minimal(&text, caller.via.clone(), now_ms()));
        drop(text);
        self.reconcile();
        let applied = applied.map_err(|e| self.core(e))?;
        self.base = Some(id);
        self.observed = Some(id.stat());
        self.meta = meta;
        self.buffer.mark_saved();
        self.disk = if self.signal.is_unwatched() { DiskState::Unwatched } else { DiskState::Clean };
        let body = match applied {
            Some(applied) => {
                self.publish_edit(&applied);
                let origin: Origin = cosmix_edit_core::origin::TOOL_DISK.parse().map_err(|e| self.core(e))?;
                json(&ReloadReply::Applied(Box::new(self.mutation_reply(&applied, &origin, false))))
            }
            None => json(&ReloadReply::Unchanged(UnchangedReply {
                buffer: self.bid.clone(),
                rev: self.buffer.rev(),
                unchanged: true,
            })),
        };
        self.push_state();
        Ok(body)
    }

    /// The watcher rang: compare the bound path with `base` (module docs of
    /// `watch`). Our own saves are swallowed by the `base` comparison.
    async fn recheck(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let before = self.disk;
        let probe = path.clone();
        let stat = match blocking(move || files::stat(&probe)).await {
            Ok(Ok(stat)) => stat,
            _ => return,
        };
        let mut disk = self.disk;
        match stat {
            None => {
                if self.base.is_some() || !matches!(self.disk, DiskState::None) {
                    disk = DiskState::Deleted;
                }
                self.observed = None;
            }
            Some(s) if self.base.map(|b| b.stat()) == Some(s) => {
                disk = DiskState::Clean;
                self.observed = Some(s);
            }
            Some(s) if self.observed == Some(s) && !matches!(self.disk, DiskState::Deleted) => {}
            Some(s) => {
                self.observed = Some(s);
                match self.read_disk().await {
                    Ok((id, text, meta)) => {
                        if self.base.map(|b| b.blake3) == Some(id.blake3) {
                            self.base = Some(id);
                            disk = DiskState::Clean;
                        } else if !self.dirty() && self.base.is_some() {
                            let peak = (text.len() + self.buffer.len()) as u64;
                            if self.lease.take(peak) {
                                let applied = heavy(|| self.buffer.reload_minimal(&text, internal_via(), now_ms()));
                                drop(text);
                                self.reconcile();
                                match applied {
                                    Ok(applied) => {
                                        if let Some(applied) = applied {
                                            self.publish_edit(&applied);
                                        }
                                        self.buffer.mark_saved();
                                        self.base = Some(id);
                                        self.meta = meta;
                                        disk = DiskState::Clean;
                                    }
                                    Err(e) => {
                                        tracing::warn!("cosmix-editd: {}: external reload refused: {e}", self.bid);
                                        disk = DiskState::Modified;
                                    }
                                }
                            } else {
                                disk = DiskState::Modified;
                            }
                        } else {
                            disk = DiskState::Modified;
                        }
                    }
                    Err(_) => disk = DiskState::Modified,
                }
            }
        }
        if matches!(disk, DiskState::Clean) && self.signal.is_unwatched() {
            disk = DiskState::Unwatched;
        }
        if matches!(disk, DiskState::Unwatched) && !self.signal.is_unwatched() {
            disk = DiskState::Clean;
        }
        let rev_before = self.pushed.as_ref().map(|p| p.rev);
        self.disk = disk;
        if disk != before || rev_before != Some(self.buffer.rev()) {
            self.publish_disk();
        }
        self.push_state();
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Mark a cached reply as a replay.
fn as_duplicate(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(mut v) => {
            v["duplicate"] = serde_json::Value::Bool(true);
            v.to_string()
        }
        Err(_) => body.to_string(),
    }
}

fn first_line(buffer: &Buffer) -> String {
    let end = buffer
        .resolve_pos(&PosSpec::LineCol { line: 2, col: None })
        .unwrap_or(buffer.len())
        .min(256);
    let src = Src::Live(buffer);
    let end = src.floor(end);
    let mut out = String::new();
    buffer.read(0..end, &mut out);
    out
}

async fn init(a: &ActorInit) -> Result<(Actor, bool), Refusal> {
    let lease = Lease { budget: a.budget.clone(), held: a.leased };
    let (buffer, meta, path, opened_as, base, disk, created, language) = match &a.init {
        Init::Scratch { language } => {
            let buffer = Buffer::new().map_err(|e| from_core(e, None))?;
            let language = language.clone().unwrap_or_else(|| "text".into());
            (buffer, FileMeta { bom: false, eol: Eol::None }, None, None, None, DiskState::None, true, language)
        }
        Init::Load { path, opened_as, create, language } => {
            let p = path.clone();
            let loaded = blocking(move || files::load(&p)).await?;
            match loaded {
                Ok(l) => {
                    let mut buffer = l.buffer;
                    buffer.mark_saved();
                    let language = language
                        .clone()
                        .unwrap_or_else(|| cosmix_edit_core::lang::detect(Some(path), &first_line(&buffer)).to_string());
                    (buffer, l.meta, Some(l.canonical), Some(opened_as.clone()), Some(l.base), DiskState::Clean, false, language)
                }
                Err(e) if *create && e.reason.as_deref() == Some(reason::FILE_NOT_FOUND) => {
                    let buffer = Buffer::new().map_err(|e| from_core(e, None))?;
                    let language = language
                        .clone()
                        .unwrap_or_else(|| cosmix_edit_core::lang::detect(Some(path), "").to_string());
                    (
                        buffer,
                        FileMeta { bom: false, eol: Eol::None },
                        Some(path.clone()),
                        Some(opened_as.clone()),
                        None,
                        DiskState::None,
                        true,
                        language,
                    )
                }
                Err(e) => return Err(e),
            }
        }
    };
    let mut actor = Actor {
        bid: a.bid.clone(),
        epoch: a.epoch.clone(),
        buffer,
        meta,
        path,
        opened_as,
        language,
        observed: base.map(|b| b.stat()),
        base,
        disk,
        dedup: DedupCache::default(),
        snapshots: VecDeque::new(),
        lease,
        publisher: a.publisher.clone(),
        to_router: a.to_router.clone(),
        signal: a.signal.clone(),
        snapshot_seq: a.snapshot_seq.clone(),
        origin_last: None,
        pushed: None,
    };
    actor.reconcile();
    Ok((actor, created))
}

/// The actor task. Reports `Loaded`/`Failed` first, then serves its inbox and
/// disk signal strictly in order until closed.
pub async fn run(mut a: ActorInit) {
    let (mut actor, created) = match init(&a).await {
        Ok(ok) => ok,
        Err(refusal) => {
            let _ = a.to_router.send(ToRouter::Failed { bid: a.bid.clone(), refusal });
            return;
        }
    };
    let props = actor.props();
    actor.pushed = Some(props.clone());
    let _ = a.to_router.send(ToRouter::Loaded { bid: a.bid.clone(), props, created });
    let signal = a.signal.clone();
    loop {
        tokio::select! {
            biased;
            msg = a.rx.recv() => match msg {
                None => break,
                Some(ActorMsg::Cmd { verb, caller, reply }) => {
                    let body = serve(&mut actor, verb, &caller).await;
                    let _ = reply.send(body);
                }
                Some(ActorMsg::Close { force, reply }) => {
                    if actor.dirty() && !force {
                        let r = refusal(
                            ErrorCode::Conflict,
                            Some(reason::DIRTY),
                            format!(
                                "buffer {} has unsaved changes; save, or close with force:true (discards them)",
                                actor.bid
                            ),
                        )
                        .buffer(&actor.bid)
                        .rev(actor.buffer.rev());
                        let _ = a.to_router.send(ToRouter::CloseDecided { bid: actor.bid.clone(), result: Err(r), reply });
                    } else {
                        let _ = a.to_router.send(ToRouter::CloseDecided { bid: actor.bid.clone(), result: Ok(()), reply });
                        break;
                    }
                }
            },
            _ = signal.notify.notified() => {}
        }
        if signal.take() {
            actor.recheck().await;
        }
    }
}

/// Dedup, then the verb (plan §3.5 ordering: dedup before CAS).
async fn serve(actor: &mut Actor, verb: BufVerb, caller: &Caller) -> Reply {
    let key = verb.op_id().map(|op_id| DedupKey {
        caller: caller.key.clone(),
        origin: caller.origin.to_string(),
        verb: verb.verb().to_string(),
        op_id: op_id.to_string(),
    });
    if let Some(key) = &key
        && let Some(body) = actor.dedup.hit(key)
    {
        return (0, as_duplicate(&body));
    }
    match actor.handle(verb, caller).await {
        Ok(body) => {
            if let Some(key) = key {
                actor.dedup.insert(key, body.clone());
            }
            (0, body)
        }
        Err(r) => render(&r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(op: &str) -> DedupKey {
        DedupKey { caller: CallerKey::Anon, origin: "agent:anon".into(), verb: "edit.undo".into(), op_id: op.into() }
    }

    #[test]
    fn dedup_is_lru_bounded_and_keyed_by_caller() {
        let mut cache = DedupCache::default();
        for i in 0..DEDUP_ENTRIES {
            cache.insert(key(&format!("k{i}")), format!("{i}"));
        }
        assert_eq!(cache.hit(&key("k0")).as_deref(), Some("0")); // refresh k0
        cache.insert(key("new"), "n".into());
        assert_eq!(cache.entries.len(), DEDUP_ENTRIES);
        assert!(cache.get(&key("k1")).is_none(), "the least recently used entry went");
        assert!(cache.get(&key("k0")).is_some());
        let mut other = key("k0");
        other.caller = CallerKey::Local("ced".into());
        assert!(cache.get(&other).is_none(), "another caller's op_id is a different key");
    }

    #[test]
    fn duplicate_flag_is_set_on_replay() {
        let body = r#"{"rev":4,"duplicate":false}"#;
        let v: serde_json::Value = serde_json::from_str(&as_duplicate(body)).unwrap();
        assert_eq!(v["duplicate"], true);
        assert_eq!(v["rev"], 4);
    }

    #[test]
    fn snapshot_positions_and_pages() {
        let s = "ab\r\ncd\n";
        assert_eq!(str_point(s, 3), Point { offset: 3, line: 1, col: 4 });
        assert_eq!(str_point(s, 4), Point { offset: 4, line: 2, col: 1 });
        assert_eq!(str_range(s, &RangeSpec::Lines { lines: [2, 2] }).unwrap(), 4..7);
        assert_eq!(str_pos(s, &PosSpec::LineCol { line: 1, col: Some(3) }).unwrap(), 2);
        assert_eq!(
            str_pos(s, &PosSpec::LineCol { line: 9, col: None }).unwrap_err().reason.as_deref(),
            Some("line_out_of_range")
        );
        let (_, lines, start, end, truncated) = page(&Src::Snap(s), 0..s.len(), true);
        let lines = lines.unwrap();
        assert_eq!(lines.len(), 3, "two lines plus the empty last line");
        assert_eq!(lines[0].text, "ab");
        assert_eq!((start.offset, end.offset, truncated), (0, 7, false));
    }

    #[test]
    fn long_line_pages_with_cont() {
        let line = "é".repeat(MAX_REPLY_BYTES); // 2 bytes each: twice the budget
        let s = format!("{line}\nz");
        let (_, lines, _, end, truncated) = page(&Src::Snap(&s), 0..s.len(), true);
        assert!(truncated);
        let first = lines.unwrap();
        assert_eq!(first.len(), 1);
        assert!(!first[0].cont);
        assert!(s.is_char_boundary(end.offset));
        let (_, lines, start, _, _) = page(&Src::Snap(&s), end.offset..s.len(), true);
        let second = lines.unwrap();
        assert!(second[0].cont, "a mid-line page continues the line");
        assert_eq!(second[0].line, 1);
        assert_eq!(start.line, 1);
    }

    #[test]
    fn control_characters_count_their_escapes() {
        let s = "\u{1}".repeat(MAX_REPLY_BYTES / 4);
        let (text, _, _, end, truncated) = page(&Src::Snap(&s), 0..s.len(), false);
        let encoded = serde_json::to_string(&text.unwrap()).unwrap().len();
        assert!(encoded <= MAX_REPLY_BYTES - GET_OVERHEAD + 2);
        assert!(truncated && end.offset < s.len());
    }
}
