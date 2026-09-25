//! The mirror: one [`Text`] (the VIEW = confirmed(rev) + in-flight + queue)
//! with local echo and single-authority OT rebase against the `edit` daemon
//! (ced E1 plan `_plan/2026-09-26-ced-e1-implementation.md` §3 — binding;
//! the rules below are its frozen summary). Pure: no Bus, no clock, no async;
//! every method returns a [`Step`]. Stage S freezes the API; Stage E1d
//! implements it.
//!
//! # Pipeline (§3.3)
//! Only in `Live`. One request in flight per buffer. Queue head is frozen
//! into an immutable [`SentRequest`] with `base_rev = self.rev` and its
//! CURRENT items; otherwise the next queued [`ServerOp`] (undo/redo/save/
//! reload without CAS; `ApplyAt` with its `expect_rev`). Selection publishing
//! never occupies the slot: only when [`Mirror::is_idle`], 200 ms after the
//! last caret move or at the moment the pipeline drains, ≤ 1 per second.
//!
//! # Folding a server edit (§3.4) — frozen check order
//! 1. `ev.rev <= self.rev` → drop (duplicate).
//! 2. `ev.base_rev != self.rev` → `suspect()` and buffer the event.
//! 3. In-flight Local op with `op_id == ev.op_id` → ack (`rev = ev.rev`,
//!    clear, add to `completed`); `ev.op_id` in `completed` → only `rev = ev.rev`.
//! 4. Everything else folds as REMOTE, transactionally: `ops` = the PRESENT
//!    pending ops (a Doomed / `present == false` in-flight op is EXCLUDED —
//!    codex N1); on clones, for each remote step `x` in order and each op `P`:
//!    `P.items` through `x` with `ThroughFirst` (exactly the server's
//!    `BaseRev` rebase; items keep request order and their own `deleted`), and
//!    `x` through the op's items AS A SET (`ot::transform_through_set`, the
//!    items as they stood before this step) with `SelfFirst` — NOT through
//!    `P.seq()`: the sequential form mis-orders a remote insert at the end of a
//!    deleted range against an insert at its start (Stage S freeze note 3,
//!    fixture 20). `Ok` → apply, commit the clones, `rev =
//!    ev.rev`. `Err(j)` → `revert_suffix(j)` on the UNTOUCHED real state and
//!    loop (the list shrinks; with no ops the fold cannot fail). One
//!    [`Conflict`] per event, accumulated across passes.
//! 5. Own Server ops matched by op_id: caret to the end of the last
//!    transformed inserted span (offset for a pure delete).
//!
//! # Replies (§3.4 table)
//! `rc 0` (full or `reply_truncated`) → `Replied{rev}`; a Local op is acked by
//! its event / history entry / own oversized resync (`resync.rev ==` replied
//! rev and `self.rev == resync.rev - 1`); if `rev <= self.rev` already, ack
//! now. **Server completion barrier**: undo/redo/apply/applied reload clear
//! the slot when the reply is in AND `self.rev >= reply.rev`; save and
//! `unchanged` reload clear on the reply. A late reply whose op_id is in
//! `completed` is a no-op. `CONFLICT` on a Local op → revert from 0;
//! `busy` → resend the IDENTICAL [`SentRequest`] after backoff (250 ms ×2, cap
//! 5 s); other refusals → revert the op and its queue, stash, notice;
//! `epoch_mismatch` → epoch change.
//!
//! # Deadlines (§3.5) — history is read from `wire.sent_at_rev`
//! Local: history has our op_id → committed (fold, ack); absent and the log
//! covers `sent_at_rev+1..` → resend the identical request; trimmed → snapshot
//! with the view in a detached copy. Undo/redo/`ApplyAt`: never resent — found
//! → fold; absent → clear + notice (a late commit folds as remote); trimmed →
//! `suspect()` + notice. Reload: `suspect()`, never resent. Save: `edit.list`
//! check, then one identical resend, then notice.
//!
//! # Recovery (§3.6)
//! Global `event_seq` gap, `resync all`, or the Bus reconnect edge → every
//! Live mirror `suspect()`s: `edit.history {since_rev: rev}` (strictly after),
//! events buffered meanwhile; snapshot fallback on `edits_elided`, trimmed
//! history or a replay mismatch — resolving the in-flight op FIRST (never
//! doomed on a guess), reverting + stashing queued ops, then paging
//! `edit.get snapshot:true`. Epoch change → `Detached{EpochChanged}`.
//!
//! # Conflicts (§3.7)
//! `revert_suffix(j)`: undo ops `j..` in reverse via [`Pending::inverse`]
//! (one `Revert` delta); an in-flight op in the suffix becomes
//! `present = false, Doomed` — refusal expected; `rc 0` = desync → snapshot
//! with a detached copy; deadline → §3.5 Local procedure without resend.

//!
//! # Driving it (E1d)
//! - After EVERY call the host drains [`Mirror::next_outgoing`] (new writes
//!   come only from the scheduler) and [`Mirror::take_retry_timer`] (a busy
//!   backoff to arm; when it fires, [`Mirror::on_retry`]). [`Step::out`]
//!   carries reads (`edit.get`, `edit.history`, `edit.list`) and identical
//!   resends only.
//! - Every reply, reads included, goes to [`Mirror::on_reply`] with the
//!   [`Outgoing::op_id`] it was sent with; reads carry a mirror-local
//!   `read-<n>` id that is not in the body. A read's deadline re-sends it.
//! - Local ops and server ops leave in the order they were queued (one FIFO
//!   across both), so an undo pressed before more typing undoes only what was
//!   typed before it and never the later typing: §3.3's "drain first",
//!   order-preserving (lead decision, 2026-09-26).
//! - After a snapshot, an in-flight op whose reply rev is past the snapshot
//!   stays in flight with `present == false`; its echo then folds as a remote
//!   edit and acks it. Its items are not in the snapshot's coordinates, so it
//!   is never re-applied optimistically (a sharpening of §3.6 step 4).
//! - History coverage uses editd's `oldest_rev` as it is defined: the rev just
//!   before the first retained entry, so `since` is covered iff
//!   `oldest_rev <= since`.
//! - Epoch change: [`Mirror::epoch_changed`] detaches; the controller opens
//!   the buffer again and calls [`Mirror::reattach`]. Different text → a
//!   [`DetachedCopy`], then [`Mirror::keep_mine`] (one `edit.replace` with
//!   `expect_rev`, or the staged transfer over 1 MiB, cut at the last newline
//!   in the second half of each 1 MiB window, else at a char boundary) or
//!   [`Mirror::take_theirs`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;

use cosmix_edit_core::anchor::{Bias, Selection, map_point};
use cosmix_edit_core::error::{ErrorCode, reason};
use cosmix_edit_core::limits::{MAX_OPS_PER_TXN, MAX_REQUEST_TEXT_BYTES};
use cosmix_edit_core::ot::{self, Edit, Priority};
use cosmix_edit_core::text::Text;
use cosmix_edit_core::wire;
use serde_json::{Value, json};

use crate::types::{Conflict, DeltaKind, Intent, Level, LocalEdit, Notice, OpIdGen, Outgoing, UI_ORIGIN, ViewDelta};

/// Acked op ids remembered for late replies and duplicate echoes.
const COMPLETED_MAX: usize = 4096;
/// Request deadlines (plan §2): 5 s, 30 s for open / get pages / save.
const DEADLINE_MS: u64 = 5_000;
const DEADLINE_LONG_MS: u64 = 30_000;
/// `edit.history` page size for recovery and reconciliation.
const HISTORY_LIMIT: usize = 1000;
/// `RESOURCE_LIMIT busy` backoff: 250 ms doubling, capped at 5 s.
const RETRY_FIRST_MS: u64 = 250;
const RETRY_CAP_MS: u64 = 5_000;

/// Notice texts a caller may match on (plan §3.5).
pub const MSG_UNDO_INCOMPLETE: &str = "Undo did not complete — press again";
pub const MSG_REDO_INCOMPLETE: &str = "Redo did not complete — press again";
pub const MSG_REPLACE_INCOMPLETE: &str = "Replace did not complete — try again";
pub const MSG_SAVE_UNCERTAIN: &str = "The save may not have completed — save again";
/// A save refused `disk_modified` (the file changed on disk): the chrome
/// asks before a forced save.
pub const MSG_SAVE_DISK_MODIFIED: &str = "The file changed on disk since it was opened — save anyway?";
pub const MSG_KEEP_INTERRUPTED: &str =
    "Transfer interrupted; the shared buffer holds a partial copy of yours — Retry, Undo remaining or Save mine as…";

/// One item of a local transaction, in REQUEST order for life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub range: Range<usize>,
    pub text: String,
    /// The text this item removes (for the inverse).
    pub deleted: String,
}

/// A local optimistic op: a base-coordinate transaction on the view text just
/// before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub op_id: String,
    pub intent: Intent,
    pub items: Vec<Item>,
    pub coalesce: bool,
}

impl Pending {
    /// Application-order steps (`ot::txn_sequence`), each with its item index.
    pub fn seq(&self) -> Vec<(usize, Edit)> {
        let pairs: Vec<(Range<usize>, String)> = self.items.iter().map(|i| (i.range.clone(), i.text.clone())).collect();
        match ot::txn_sequence(&pairs) {
            Ok(s) => s,
            Err(_) => {
                // Items are validated at creation and transforms keep them
                // disjoint; an overlap here is a bug, not an input.
                debug_assert!(false, "pending items overlap: {:?}", self.items);
                Vec::new()
            }
        }
    }

    /// Inverse steps: for the steps in REVERSE application order,
    /// `(offset, text.len(), item.deleted)`.
    pub fn inverse(&self) -> Vec<Edit> {
        self.seq()
            .into_iter()
            .rev()
            .map(|(i, e)| Edit { offset: e.offset, delete: e.insert.len(), insert: self.items[i].deleted.clone() })
            .collect()
    }
}

/// The exact bytes sent, immutable for retries (codex #3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRequest {
    pub verb: &'static str,
    pub body: String,
    pub op_id: String,
    pub base_rev: Option<u64>,
    /// `self.rev` when sent; deadline reconciliation reads history from here.
    pub sent_at_rev: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyInfo {
    pub rev: u64,
    /// `reply_truncated: true` — `base_rev` / `rebased` absent.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    Sent,
    Replied(ReplyInfo),
    /// Deadline passed with no reply.
    Uncertain,
    /// Reverted locally; its refusal is expected.
    Doomed,
}

/// The lane an undo/redo acts on (`origin` arg): own, `"*"`, or a named lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneArg {
    Own,
    Any,
    Lane(String),
}

/// A non-optimistic request (never applied locally before its event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerOp {
    Undo { lane: LaneArg },
    Redo { lane: LaneArg },
    Save { path: Option<String>, force: bool },
    Reload { force: bool },
    /// Replace / replace-all: base items at `expect_rev`.
    ApplyAt { items: Vec<(Range<usize>, String)>, expect_rev: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inflight {
    /// `present`: the op's effect is in the view text. A reverted (Doomed) op
    /// has `present == false` and is excluded from every fold.
    Local { p: Pending, present: bool, wire: SentRequest, state: AckState },
    Server { op: ServerOp, intent: Intent, wire: SentRequest, state: AckState },
}

impl Inflight {
    fn wire(&self) -> &SentRequest {
        match self {
            Inflight::Local { wire, .. } | Inflight::Server { wire, .. } => wire,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverHow {
    History,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachReason {
    EpochChanged,
    ClosedRemotely { by: Option<String> },
    OpenFailed { msg: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Bootstrapping { buffered: Vec<wire::EditEvent> },
    Live,
    Recovering { since: u64, buffered: Vec<wire::EditEvent>, how: RecoverHow },
    Detached { reason: DetachReason },
}

/// Buffer metadata mirrored from open/list/props/events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferMeta {
    pub path: Option<String>,
    pub name: Option<String>,
    pub language: String,
    pub eol: wire::Eol,
    pub bom: bool,
    pub disk: wire::DiskState,
    pub dirty: bool,
    pub saved_rev: Option<u64>,
    pub recovered: bool,
    pub recovery_id: String,
}

/// The newest edit by an origin other than ours (status bar, Ctrl+Alt+Z).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMark {
    pub origin: String,
    pub lane: String,
    pub rev: u64,
    pub kind: wire::KindW,
    /// Envelope of its transformed inserted spans (view coordinates).
    pub span: Option<Range<usize>>,
}

/// ced's text kept after an epoch change that differed, or an unknown op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedCopy {
    pub text: String,
    pub rev_seen: u64,
}

/// What a mirror call produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    pub deltas: Vec<ViewDelta>,
    pub out: Vec<Outgoing>,
    pub notices: Vec<Notice>,
}

impl Step {
    /// Append another step's output after this one's.
    pub fn extend(&mut self, other: Step) {
        self.deltas.extend(other.deltas);
        self.out.extend(other.out);
        self.notices.extend(other.notices);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorError {
    /// Not `Live` (bootstrapping, recovering or detached).
    NotLive,
    /// More than `MAX_OPS_PER_TXN` items or 1 MiB inserted (plan §3.2).
    TooLarge { items: usize, bytes: usize },
    /// Items overlap, are reversed, or leave a char boundary.
    Invalid(String),
}

// ── private state ──────────────────────────────────────────────────────────

/// What an outstanding read is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// A page of `edit.get snapshot` (bootstrap, snapshot recovery, reattach).
    Page,
    /// `edit.history` for §3.6 recovery.
    Recover,
    /// `edit.history` for §3.5 deadline reconciliation of the in-flight op.
    Recon,
    /// `edit.list` for a lost save reply.
    SaveCheck,
}

/// Snapshot pages being accumulated.
struct Pager {
    token: Option<String>,
    text: String,
}

/// §3.5 reconciliation of the in-flight op.
struct Recon {
    op_id: String,
    /// `wire.sent_at_rev`.
    since: u64,
    found: Option<u64>,
    trimmed: bool,
    first: bool,
    entries: Vec<wire::HistoryEntryW>,
    /// Save only: the identical save was already resent once.
    save_resent: bool,
}

/// A queued non-optimistic op.
struct QServer {
    op: ServerOp,
    intent: Intent,
    op_id: String,
    /// Forced verb (keep-mine stages); `None` = by shape.
    verb: Option<&'static str>,
    /// `expect_rev` for an undo (keep-mine compensation).
    expect: Option<u64>,
}

/// One step of a keep-mine transfer (plan §3.8).
#[derive(Debug, Clone)]
struct KeepStage {
    op_id: String,
    verb: &'static str,
    range: Range<usize>,
    text: String,
}

struct KeepMine {
    intent: Intent,
    /// Stages not yet queued.
    stages: VecDeque<KeepStage>,
    /// `(op_id, reply rev)` of every committed stage, in order.
    done: Vec<(String, u64)>,
    /// The op id currently queued or in flight.
    current: Option<String>,
    /// Undoing the committed stages after an abort; count undone so far.
    compensating: Option<usize>,
}

#[derive(Default)]
struct Completed {
    order: VecDeque<String>,
    all: HashSet<String>,
    local: HashSet<String>,
}

impl Completed {
    fn add(&mut self, id: String, local: bool) {
        if self.all.contains(&id) {
            return;
        }
        if self.order.len() >= COMPLETED_MAX
            && let Some(old) = self.order.pop_front()
        {
            self.all.remove(&old);
            self.local.remove(&old);
        }
        if local {
            self.local.insert(id.clone());
        }
        self.all.insert(id.clone());
        self.order.push_back(id);
    }
}

/// Busy backoff: the identical request goes again after `delay`.
struct Retry {
    next_delay: u64,
    /// A timer the host has not armed yet.
    unarmed: Option<u64>,
    waiting: bool,
}

/// A server edit to fold: an event, or a history entry as one.
struct Ev<'a> {
    rev: u64,
    base_rev: u64,
    origin: &'a str,
    lane: &'a str,
    kind: wire::KindW,
    op_id: Option<&'a str>,
    edits: &'a [Edit],
}

impl<'a> Ev<'a> {
    fn of_event(e: &'a wire::EditEvent) -> Self {
        Ev {
            rev: e.rev,
            base_rev: e.base_rev,
            origin: &e.origin,
            lane: &e.lane,
            kind: e.kind,
            op_id: e.op_id.as_deref(),
            edits: &e.edits,
        }
    }
}

/// Apply edits in application order: one two-phase commit when the sequence
/// is canonical, else one per step (inverses and transformed remotes need not
/// be canonical).
fn apply_text(text: &mut Text, edits: &[Edit]) -> Result<(), String> {
    if edits.is_empty() {
        return Ok(());
    }
    let canonical = edits.windows(2).all(|w| w[1].offset + w[1].delete <= w[0].offset);
    if canonical {
        let p = text.prepare(edits.to_vec()).map_err(|e| e.to_string())?;
        text.commit(p);
        return Ok(());
    }
    for e in edits {
        let p = text.prepare(vec![e.clone()]).map_err(|e| e.to_string())?;
        text.commit(p);
    }
    Ok(())
}

fn read_all(text: &Text) -> String {
    let mut s = String::with_capacity(text.len());
    text.read(0..text.len(), &mut s);
    s
}

fn is_own_origin(origin: &str) -> bool {
    origin == UI_ORIGIN || origin.starts_with("agent:ced.")
}

fn delta_kind(k: wire::KindW) -> DeltaKind {
    match k {
        wire::KindW::Edit => DeltaKind::Remote,
        wire::KindW::Undo => DeltaKind::Undo,
        wire::KindW::Redo => DeltaKind::Redo,
        wire::KindW::Reload => DeltaKind::Reload,
    }
}

/// Cut `s` into pieces of at most `max` bytes at char boundaries, preferring
/// the last newline in the second half of each window (keep-mine stages).
fn chunks(s: &str, max: usize, at_newlines: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if at_newlines
            && let Some(nl) = rest.as_bytes()[..cut].iter().rposition(|&b| b == b'\n')
            && nl + 1 >= max / 2
        {
            cut = nl + 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

fn meta_of(open: &wire::OpenReply) -> BufferMeta {
    BufferMeta {
        path: open.path.clone(),
        name: open.name.clone(),
        language: open.language.clone(),
        eol: open.eol,
        bom: open.bom,
        disk: open.disk,
        dirty: open.recovered,
        // E0's open reply carries no saved_rev: a freshly opened, unrecovered
        // buffer is taken as saved at its rev (the controller refreshes from
        // `edit.list`).
        saved_rev: (!open.recovered).then_some(open.rev),
        recovered: open.recovered,
        recovery_id: open.recovery_id.clone(),
    }
}

/// See the module docs.
pub struct Mirror {
    buffer: wire::BufferId,
    epoch: String,
    text: Text,
    rev: u64,
    view_gen: u64,
    inflight: Option<Inflight>,
    /// Local ops, each with its FIFO position (shared with `server_ops`).
    queue: VecDeque<(u64, Pending)>,
    server_ops: VecDeque<(u64, QServer)>,
    fifo: u64,
    conflicts: Vec<Conflict>,
    detached_copy: Option<DetachedCopy>,
    meta: BufferMeta,
    last_remote: Option<RemoteMark>,
    phase: Phase,
    completed: Completed,
    reads: HashMap<String, (Read, Outgoing)>,
    read_seq: u64,
    pager: Option<Pager>,
    /// A snapshot fallback is due; `true` = keep the view as a detached copy.
    snap_due: Option<bool>,
    recon: Option<Recon>,
    retry: Option<Retry>,
    /// Loss noticed while not Live: recover again once Live.
    resuspect: bool,
    /// Reattach: the view text to compare with the service's.
    compare_with: Option<String>,
    keep: Option<KeepMine>,
    keep_count: u32,
    cursors: Vec<(String, Vec<Selection>)>,
}

impl Mirror {
    /// From an `edit.open` reply: `Bootstrapping`, and the first
    /// `edit.get {snapshot:true}` to send.
    pub fn bootstrap(open: &wire::OpenReply) -> Result<(Mirror, Step), MirrorError> {
        let text = Text::new().map_err(|e| MirrorError::Invalid(e.to_string()))?;
        let mut m = Mirror {
            buffer: open.buffer.clone(),
            epoch: open.epoch.clone(),
            text,
            rev: open.rev,
            view_gen: 0,
            inflight: None,
            queue: VecDeque::new(),
            server_ops: VecDeque::new(),
            fifo: 0,
            conflicts: Vec::new(),
            detached_copy: None,
            meta: meta_of(open),
            last_remote: None,
            phase: Phase::Bootstrapping { buffered: Vec::new() },
            completed: Completed::default(),
            reads: HashMap::new(),
            read_seq: 0,
            pager: None,
            snap_due: None,
            recon: None,
            retry: None,
            resuspect: false,
            compare_with: None,
            keep: None,
            keep_count: 0,
            cursors: Vec::new(),
        };
        let mut step = Step::default();
        m.start_pages(&mut step);
        Ok((m, step))
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }
    pub fn epoch(&self) -> &str {
        &self.epoch
    }
    pub fn text(&self) -> &Text {
        &self.text
    }
    pub fn rev(&self) -> u64 {
        self.rev
    }
    /// View generation: +1 on every [`ViewDelta`] (async result identity, §4.3).
    pub fn view_gen(&self) -> u64 {
        self.view_gen
    }
    pub fn phase(&self) -> &Phase {
        &self.phase
    }
    pub fn meta(&self) -> &BufferMeta {
        &self.meta
    }
    pub fn conflicts(&self) -> &[Conflict] {
        &self.conflicts
    }
    pub fn detached_copy(&self) -> Option<&DetachedCopy> {
        self.detached_copy.as_ref()
    }
    pub fn last_remote(&self) -> Option<&RemoteMark> {
        self.last_remote.as_ref()
    }
    /// Number of local ops not yet acknowledged (in flight + queued).
    pub fn pending(&self) -> usize {
        self.queue.len() + matches!(self.inflight, Some(Inflight::Local { .. })) as usize
    }
    pub fn inflight(&self) -> Option<&Inflight> {
        self.inflight.as_ref()
    }
    /// `Live`, nothing in flight, empty queue, no server ops waiting.
    pub fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Live)
            && self.inflight.is_none()
            && self.queue.is_empty()
            && self.server_ops.is_empty()
    }
    /// Other origins' selections from `cursor` events, mapped into the view.
    pub fn remote_cursors(&self) -> &[(String, Vec<Selection>)] {
        &self.cursors
    }
    /// A keep-mine transfer (or its compensation) is running.
    pub fn keep_in_progress(&self) -> bool {
        self.keep.is_some()
    }

    /// Apply an optimistic local edit now (one `Local` delta) and queue it.
    /// A single item inserting more than 1 MiB (a paste) is split into
    /// consecutive ≤ 1 MiB ops; a multi-item edit over the limits is refused
    /// with nothing applied.
    pub fn local_edit(&mut self, e: LocalEdit, intent: Intent, ids: &mut OpIdGen) -> Result<Step, MirrorError> {
        if !matches!(self.phase, Phase::Live) {
            return Err(MirrorError::NotLive);
        }
        let mut step = Step::default();
        if e.items.is_empty() {
            return Ok(step);
        }
        let bytes: usize = e.items.iter().map(|(_, t)| t.len()).sum();
        if e.items.len() > MAX_OPS_PER_TXN || (bytes > MAX_REQUEST_TEXT_BYTES && e.items.len() > 1) {
            return Err(MirrorError::TooLarge { items: e.items.len(), bytes });
        }
        let len = self.text.len();
        for (r, _) in &e.items {
            if r.start > r.end || r.end > len {
                return Err(MirrorError::Invalid(format!("range [{}, {}) outside 0..{len}", r.start, r.end)));
            }
            if !self.text.is_char_boundary(r.start) || !self.text.is_char_boundary(r.end) {
                return Err(MirrorError::Invalid(format!("range [{}, {}) is not on char boundaries", r.start, r.end)));
            }
        }
        if ot::txn_sequence(&e.items).is_err() {
            return Err(MirrorError::Invalid("items overlap".into()));
        }
        let mut pendings = Vec::new();
        if bytes > MAX_REQUEST_TEXT_BYTES {
            let (r, text) = &e.items[0];
            let mut deleted = String::new();
            self.text.read(r.clone(), &mut deleted);
            let mut at = r.start;
            for (k, piece) in chunks(text, MAX_REQUEST_TEXT_BYTES, false).into_iter().enumerate() {
                let (range, deleted) = if k == 0 { (r.clone(), std::mem::take(&mut deleted)) } else { (at..at, String::new()) };
                at = range.start + piece.len();
                pendings.push(Pending {
                    op_id: ids.next_id(),
                    intent: intent.clone(),
                    items: vec![Item { range, text: piece.to_string(), deleted }],
                    coalesce: false,
                });
            }
        } else {
            let items = e
                .items
                .iter()
                .map(|(r, t)| {
                    let mut deleted = String::new();
                    self.text.read(r.clone(), &mut deleted);
                    Item { range: r.clone(), text: t.clone(), deleted }
                })
                .collect();
            pendings.push(Pending { op_id: ids.next_id(), intent: intent.clone(), items, coalesce: e.coalesce });
        }
        let mut edits = Vec::new();
        for p in &pendings {
            let seq: Vec<Edit> = p.seq().into_iter().map(|(_, e)| e).collect();
            apply_text(&mut self.text, &seq).map_err(MirrorError::Invalid)?;
            edits.extend(seq);
        }
        for p in pendings {
            self.fifo += 1;
            self.queue.push_back((self.fifo, p));
        }
        self.meta.dirty = true;
        self.push_delta(&mut step, edits, Some(intent.origin), DeltaKind::Local);
        Ok(step)
    }

    /// Queue a non-optimistic request (sent when the pipeline is empty).
    pub fn server_op(&mut self, op: ServerOp, intent: Intent, ids: &mut OpIdGen) -> Step {
        let op_id = ids.next_id();
        self.queue_server(QServer { op, intent, op_id, verb: None, expect: None });
        Step::default()
    }

    /// An `edit.changed` event for THIS buffer (the controller filters by
    /// buffer and tracks the global `event_seq`).
    pub fn on_event(&mut self, ev: &wire::Event) -> Step {
        let mut step = Step::default();
        match ev {
            wire::Event::Edit(e) if e.buffer == self.buffer => {
                if e.epoch != self.epoch {
                    return self.epoch_changed();
                }
                self.on_edit_event(e, &mut step);
            }
            wire::Event::Cursor(c) if c.buffer == self.buffer => self.on_cursor(c),
            wire::Event::Disk(d) if d.buffer == self.buffer => self.meta.disk = d.disk,
            wire::Event::Close(c) if c.buffer == self.buffer => {
                self.phase = Phase::Detached { reason: DetachReason::ClosedRemotely { by: None } };
                self.abandon_pipeline();
            }
            wire::Event::Resync(r) => {
                if r.epoch != self.epoch {
                    return self.epoch_changed();
                }
                self.on_resync(r, &mut step);
            }
            _ => {}
        }
        step
    }

    /// The reply to one of this mirror's requests, matched by the op_id the
    /// [`Outgoing`] carried (a `read-<n>` id for reads).
    pub fn on_reply(&mut self, op_id: &str, reply: Result<Value, wire::Refusal>) -> Step {
        if let Some((kind, _)) = self.reads.remove(op_id) {
            return self.on_read_reply(kind, reply);
        }
        let mut step = Step::default();
        if self.completed.all.contains(op_id) {
            return step;
        }
        if self.inflight.as_ref().map(|i| i.wire().op_id.as_str()) != Some(op_id) {
            return step;
        }
        match reply {
            Ok(v) => {
                if let Some(ep) = v.get("epoch").and_then(Value::as_str)
                    && ep != self.epoch
                {
                    return self.epoch_changed();
                }
                self.reply_ok(&v, &mut step);
            }
            Err(r) => self.reply_refused(&r, &mut step),
        }
        self.try_snapshot(&mut step);
        step
    }

    /// The request carrying `op_id` passed its deadline with no reply (§3.5).
    pub fn on_deadline(&mut self, op_id: &str) -> Step {
        let mut step = Step::default();
        if let Some((_, out)) = self.reads.get(op_id) {
            // Reads are idempotent: ask again.
            step.out.push(out.clone());
            return step;
        }
        let Some(inflight) = self.inflight.as_mut() else { return step };
        if inflight.wire().op_id != op_id {
            return step;
        }
        if self.retry.as_ref().is_some_and(|r| r.waiting) {
            return step;
        }
        match inflight {
            Inflight::Local { state, .. } => match state {
                AckState::Sent => {
                    *state = AckState::Uncertain;
                    self.start_recon(&mut step);
                }
                AckState::Doomed => {
                    if self.recon.is_none() {
                        self.start_recon(&mut step);
                    }
                }
                AckState::Uncertain | AckState::Replied(_) => {}
            },
            Inflight::Server { op, state, .. } => {
                if !matches!(state, AckState::Sent) {
                    return step;
                }
                match op {
                    ServerOp::Reload { .. } => {
                        // Reload entries carry no op_id: history folds whatever
                        // happened; never resent.
                        self.clear_slot();
                        step.extend(self.suspect());
                    }
                    ServerOp::Save { .. } => {
                        if self.recon.as_ref().is_some_and(|r| r.save_resent) {
                            self.recon = None;
                            self.clear_slot();
                            step.notices.push(Notice::Message { level: Level::Warn, text: MSG_SAVE_UNCERTAIN.into() });
                        } else {
                            *state = AckState::Uncertain;
                            self.recon = Some(Recon {
                                op_id: op_id.to_string(),
                                since: 0,
                                found: None,
                                trimmed: false,
                                first: true,
                                entries: Vec::new(),
                                save_resent: false,
                            });
                            self.send_read(Read::SaveCheck, "edit.list", json!({}), DEADLINE_MS, &mut step);
                        }
                    }
                    _ => {
                        *state = AckState::Uncertain;
                        self.start_recon(&mut step);
                    }
                }
            }
        }
        step
    }

    /// One page of `edit.get snapshot:true` (bootstrap, snapshot recovery,
    /// reattach).
    pub fn on_page(&mut self, page: &wire::GetReply) -> Step {
        let mut step = Step::default();
        if page.buffer != self.buffer {
            return step;
        }
        if page.epoch != self.epoch {
            return self.epoch_changed();
        }
        let Some(pager) = self.pager.as_mut() else { return step };
        if let Some(t) = &page.text {
            pager.text.push_str(t);
        }
        if page.snapshot.is_some() {
            pager.token = page.snapshot.clone();
        }
        if let Some(next) = page.next {
            let Some(token) = pager.token.clone() else {
                // Paged without a snapshot token: start over.
                self.start_pages(&mut step);
                return step;
            };
            let body = json!({"buffer": self.buffer, "snapshot": token, "range": [next, page.bytes_total]});
            self.send_read(Read::Page, "edit.get", body, DEADLINE_LONG_MS, &mut step);
            return step;
        }
        let text = self.pager.take().map(|p| p.text).unwrap_or_default();
        self.finish_snapshot(text, page.rev, &mut step);
        step
    }

    /// One page of `edit.history` (recovery or deadline reconciliation).
    pub fn on_history(&mut self, page: &wire::HistoryReply) -> Step {
        let kind = if matches!(self.phase, Phase::Recovering { how: RecoverHow::History, .. }) {
            Read::Recover
        } else {
            Read::Recon
        };
        let mut step = Step::default();
        self.history_page(kind, page, &mut step);
        step
    }

    /// Possible loss: recover from history (§3.6).
    pub fn suspect(&mut self) -> Step {
        let mut step = Step::default();
        if !matches!(self.phase, Phase::Live) {
            if !matches!(self.phase, Phase::Detached { .. }) {
                self.resuspect = true;
            }
            return step;
        }
        self.phase = Phase::Recovering { since: self.rev, buffered: Vec::new(), how: RecoverHow::History };
        let body = json!({"buffer": self.buffer, "since_rev": self.rev, "limit": HISTORY_LIMIT});
        self.send_read(Read::Recover, "edit.history", body, DEADLINE_MS, &mut step);
        step
    }

    /// The scheduler (§3.3): the next request to send, if any.
    pub fn next_outgoing(&mut self) -> Option<Outgoing> {
        if !matches!(self.phase, Phase::Live) || self.inflight.is_some() {
            return None;
        }
        let local_first = match (self.queue.front(), self.server_ops.front()) {
            (Some((a, _)), Some((b, _))) => a < b,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => return None,
        };
        let (wire, deadline) = if local_first {
            let (_, p) = self.queue.pop_front()?;
            let (verb, body) = self.local_body(&p);
            let wire = SentRequest {
                verb,
                body: body.to_string(),
                op_id: p.op_id.clone(),
                base_rev: Some(self.rev),
                sent_at_rev: self.rev,
            };
            self.inflight = Some(Inflight::Local { p, present: true, wire: wire.clone(), state: AckState::Sent });
            (wire, DEADLINE_MS)
        } else {
            let (_, q) = self.server_ops.pop_front()?;
            let (verb, body) = self.server_body(&q);
            let deadline = if verb == "edit.save" { DEADLINE_LONG_MS } else { DEADLINE_MS };
            let wire = SentRequest { verb, body: body.to_string(), op_id: q.op_id.clone(), base_rev: None, sent_at_rev: self.rev };
            self.inflight = Some(Inflight::Server { op: q.op, intent: q.intent, wire: wire.clone(), state: AckState::Sent });
            (wire, deadline)
        };
        Some(Outgoing { verb: wire.verb.to_string(), body: wire.body, op_id: Some(wire.op_id), deadline_ms: deadline })
    }

    /// A busy backoff the host must arm as a one-shot timer (then call
    /// [`Mirror::on_retry`]). Handed out once per backoff.
    pub fn take_retry_timer(&mut self) -> Option<u64> {
        self.retry.as_mut().and_then(|r| r.unarmed.take())
    }

    /// The busy backoff timer fired: send the IDENTICAL request again.
    pub fn on_retry(&mut self) -> Step {
        let mut step = Step::default();
        let Some(retry) = self.retry.as_mut() else { return step };
        if !retry.waiting {
            return step;
        }
        retry.waiting = false;
        if let Some(inflight) = &self.inflight {
            step.out.push(Self::resend(inflight.wire()));
        }
        step
    }

    /// The daemon's epoch changed (event/reply epoch, `epoch_mismatch`, or
    /// `edit` re-registered): detach; the controller reopens the buffer and
    /// calls [`Mirror::reattach`]. The view text is kept.
    pub fn epoch_changed(&mut self) -> Step {
        if !matches!(self.phase, Phase::Detached { reason: DetachReason::EpochChanged }) {
            self.phase = Phase::Detached { reason: DetachReason::EpochChanged };
            self.abandon_pipeline();
        }
        Step::default()
    }

    /// Reattach to the buffer as the new daemon session reopened it (plan
    /// §3.8): page a snapshot and compare it with the view. Equal → Live with
    /// no change; different → Live on the service's text, the view kept as a
    /// [`DetachedCopy`] (+ [`Notice::DetachedCopy`]).
    pub fn reattach(&mut self, open: &wire::OpenReply) -> Step {
        let mut step = Step::default();
        self.compare_with = Some(read_all(&self.text));
        self.buffer = open.buffer.clone();
        self.epoch = open.epoch.clone();
        self.rev = open.rev;
        self.meta = meta_of(open);
        self.abandon_pipeline();
        self.completed = Completed::default();
        self.cursors.clear();
        self.phase = Phase::Bootstrapping { buffered: Vec::new() };
        self.start_pages(&mut step);
        step
    }

    /// "Keep mine" (plan §3.8): write the detached copy back over the
    /// service's text as the minimal differing span — one `edit.replace` with
    /// `expect_rev` up to 1 MiB, else a staged delete + ≤ 1 MiB inserts
    /// chained by `expect_rev`, each its own undo group, op ids
    /// `c<run>-keep<k>-<seq>`. An abort compensates with own-lane undos.
    pub fn keep_mine(&mut self, intent: Intent, ids: &mut OpIdGen) -> Step {
        let mut step = Step::default();
        let Some(copy) = self.detached_copy.as_ref() else { return step };
        if !self.is_idle() || self.keep.is_some() {
            step.notices.push(Notice::Message {
                level: Level::Warn,
                text: "Keep mine waits until the buffer is idle — try again".into(),
            });
            return step;
        }
        let mine = copy.text.clone();
        let theirs = read_all(&self.text);
        let (a, b, ins) = {
            let (m, t) = (mine.as_bytes(), theirs.as_bytes());
            let mut p = m.iter().zip(t).take_while(|(x, y)| x == y).count();
            while !mine.is_char_boundary(p) || !theirs.is_char_boundary(p) {
                p -= 1;
            }
            let max_s = (m.len() - p).min(t.len() - p);
            let mut s = m.iter().rev().zip(t.iter().rev()).take(max_s).take_while(|(x, y)| x == y).count();
            while !mine.is_char_boundary(m.len() - s) || !theirs.is_char_boundary(t.len() - s) {
                s -= 1;
            }
            (p, t.len() - s, mine[p..m.len() - s].to_string())
        };
        if a == b && ins.is_empty() {
            self.detached_copy = None;
            return step;
        }
        self.keep_count += 1;
        let tag = format!("keep{}", self.keep_count);
        let mut stages = VecDeque::new();
        if ins.len() <= MAX_REQUEST_TEXT_BYTES {
            stages.push_back(KeepStage { op_id: ids.next_tagged(&tag), verb: "edit.replace", range: a..b, text: ins });
        } else {
            if b > a {
                stages.push_back(KeepStage { op_id: ids.next_tagged(&tag), verb: "edit.delete", range: a..b, text: String::new() });
            }
            let mut at = a;
            for piece in chunks(&ins, MAX_REQUEST_TEXT_BYTES, true) {
                stages.push_back(KeepStage {
                    op_id: ids.next_tagged(&tag),
                    verb: "edit.insert",
                    range: at..at,
                    text: piece.to_string(),
                });
                at += piece.len();
            }
        }
        self.keep = Some(KeepMine { intent, stages, done: Vec::new(), current: None, compensating: None });
        let rev = self.rev;
        self.keep_next_stage(rev);
        step
    }

    /// "Take the service's": drop the detached copy.
    pub fn take_theirs(&mut self) -> Step {
        self.detached_copy = None;
        Step::default()
    }

    /// Forget the recorded conflict(s) of remote rev `rev` (the infobar's
    /// dismiss). `true` when one was removed.
    pub fn dismiss_conflict(&mut self, rev: u64) -> bool {
        let before = self.conflicts.len();
        self.conflicts.retain(|c| c.rev != rev);
        self.conflicts.len() != before
    }

    // ── internals ────────────────────────────────────────────────────────────

    fn push_delta(&mut self, step: &mut Step, edits: Vec<Edit>, origin: Option<cosmix_edit_core::origin::Origin>, kind: DeltaKind) {
        self.view_gen += 1;
        step.deltas.push(ViewDelta { edits, origin, kind, rev: self.rev, view_gen: self.view_gen });
    }

    fn send_read(&mut self, kind: Read, verb: &str, body: Value, deadline_ms: u64, step: &mut Step) {
        if matches!(kind, Read::Page | Read::Recover) {
            // A new read of this kind supersedes any outstanding one.
            self.reads.retain(|_, (k, _)| *k != kind);
        }
        self.read_seq += 1;
        let id = format!("read-{}", self.read_seq);
        let out = Outgoing { verb: verb.to_string(), body: body.to_string(), op_id: Some(id.clone()), deadline_ms };
        self.reads.insert(id, (kind, out.clone()));
        step.out.push(out);
    }

    fn start_pages(&mut self, step: &mut Step) {
        self.pager = Some(Pager { token: None, text: String::new() });
        let body = json!({"buffer": self.buffer, "snapshot": true});
        self.send_read(Read::Page, "edit.get", body, DEADLINE_LONG_MS, step);
    }

    fn resend(wire: &SentRequest) -> Outgoing {
        let deadline = if wire.verb == "edit.save" { DEADLINE_LONG_MS } else { DEADLINE_MS };
        Outgoing { verb: wire.verb.to_string(), body: wire.body.clone(), op_id: Some(wire.op_id.clone()), deadline_ms: deadline }
    }

    fn queue_server(&mut self, q: QServer) {
        self.fifo += 1;
        self.server_ops.push_back((self.fifo, q));
    }

    /// Drop everything in the pipeline (epoch change, remote close, reattach).
    fn abandon_pipeline(&mut self) {
        self.inflight = None;
        self.queue.clear();
        self.server_ops.clear();
        self.reads.clear();
        self.pager = None;
        self.snap_due = None;
        self.recon = None;
        self.retry = None;
        self.resuspect = false;
        self.keep = None;
    }

    fn present_inflight(&self) -> bool {
        matches!(self.inflight, Some(Inflight::Local { present: true, .. }))
    }

    fn local_body(&self, p: &Pending) -> (&'static str, Value) {
        let origin = p.intent.origin.to_string();
        let base_rev = self.rev;
        if let [it] = p.items.as_slice() {
            if it.range.is_empty() {
                return (
                    "edit.insert",
                    json!({"buffer": self.buffer, "at": it.range.start, "text": it.text, "base_rev": base_rev,
                           "coalesce": p.coalesce, "origin": origin, "op_id": p.op_id}),
                );
            }
            if it.text.is_empty() {
                return (
                    "edit.delete",
                    json!({"buffer": self.buffer, "range": [it.range.start, it.range.end], "base_rev": base_rev,
                           "coalesce": p.coalesce, "origin": origin, "op_id": p.op_id}),
                );
            }
            return (
                "edit.replace",
                json!({"buffer": self.buffer, "range": [it.range.start, it.range.end], "text": it.text,
                       "base_rev": base_rev, "coalesce": p.coalesce, "origin": origin, "op_id": p.op_id}),
            );
        }
        let ops: Vec<Value> = p.items.iter().map(|it| op_json(&it.range, &it.text)).collect();
        (
            "edit.apply",
            json!({"buffer": self.buffer, "ops": ops, "base_rev": base_rev, "coalesce": false, "origin": origin, "op_id": p.op_id}),
        )
    }

    fn server_body(&self, q: &QServer) -> (&'static str, Value) {
        let origin = q.intent.origin.to_string();
        match &q.op {
            ServerOp::Undo { lane } | ServerOp::Redo { lane } => {
                let verb = if matches!(q.op, ServerOp::Undo { .. }) { "edit.undo" } else { "edit.redo" };
                let mut body = json!({"buffer": self.buffer, "as": origin, "op_id": q.op_id});
                match lane {
                    LaneArg::Own => {}
                    LaneArg::Any => body["origin"] = json!("*"),
                    LaneArg::Lane(l) => body["origin"] = json!(l),
                }
                if let Some(x) = q.expect {
                    body["expect_rev"] = json!(x);
                }
                (verb, body)
            }
            ServerOp::Save { path, force } => {
                let mut body = json!({"buffer": self.buffer, "origin": origin, "op_id": q.op_id});
                if let Some(p) = path {
                    body["path"] = json!(p);
                }
                if *force {
                    body["force"] = json!(true);
                }
                ("edit.save", body)
            }
            ServerOp::Reload { force } => {
                ("edit.reload", json!({"buffer": self.buffer, "force": force, "origin": origin, "op_id": q.op_id}))
            }
            ServerOp::ApplyAt { items, expect_rev } => {
                if let [(r, t)] = items.as_slice() {
                    let verb = q.verb.unwrap_or(if r.is_empty() {
                        "edit.insert"
                    } else if t.is_empty() {
                        "edit.delete"
                    } else {
                        "edit.replace"
                    });
                    let body = match verb {
                        "edit.insert" => json!({"buffer": self.buffer, "at": r.start, "text": t, "expect_rev": expect_rev,
                                                "origin": origin, "op_id": q.op_id}),
                        "edit.delete" => json!({"buffer": self.buffer, "range": [r.start, r.end], "expect_rev": expect_rev,
                                                "origin": origin, "op_id": q.op_id}),
                        _ => json!({"buffer": self.buffer, "range": [r.start, r.end], "text": t, "expect_rev": expect_rev,
                                    "origin": origin, "op_id": q.op_id}),
                    };
                    return (verb, body);
                }
                let ops: Vec<Value> = items.iter().map(|(r, t)| op_json(r, t)).collect();
                (
                    "edit.apply",
                    json!({"buffer": self.buffer, "ops": ops, "expect_rev": expect_rev, "origin": origin, "op_id": q.op_id}),
                )
            }
        }
    }

    fn on_edit_event(&mut self, e: &wire::EditEvent, step: &mut Step) {
        match &mut self.phase {
            Phase::Bootstrapping { buffered } | Phase::Recovering { buffered, .. } => {
                buffered.push(e.clone());
                return;
            }
            Phase::Detached { .. } => return,
            Phase::Live => {}
        }
        // §3.4 frozen order: duplicate first, then gap.
        if e.rev <= self.rev {
            return;
        }
        if e.base_rev != self.rev {
            step.extend(self.suspect());
            if let Phase::Recovering { buffered, .. } = &mut self.phase {
                buffered.push(e.clone());
            }
            return;
        }
        self.fold_edit(&Ev::of_event(e), step);
    }

    fn on_cursor(&mut self, c: &wire::CursorEvent) {
        if !matches!(self.phase, Phase::Live) || c.rev != self.rev || is_own_origin(&c.origin) {
            return;
        }
        let mut edits: Vec<Edit> = Vec::new();
        if let Some(Inflight::Local { p, present: true, .. }) = &self.inflight {
            edits.extend(p.seq().into_iter().map(|(_, e)| e));
        }
        for (_, p) in &self.queue {
            edits.extend(p.seq().into_iter().map(|(_, e)| e));
        }
        let map = |mut a: usize| {
            for e in &edits {
                a = map_point(a, Bias::Before, e).0;
            }
            a
        };
        let sels: Vec<Selection> =
            c.selections.iter().map(|s| Selection { anchor: map(s.anchor), head: map(s.head) }).collect();
        self.cursors.retain(|(o, _)| o != &c.origin);
        if !sels.is_empty() {
            self.cursors.push((c.origin.clone(), sels));
        }
    }

    fn on_resync(&mut self, r: &wire::ResyncEvent, step: &mut Step) {
        match &r.buffers {
            wire::ResyncTarget::All(_) => step.extend(self.suspect()),
            wire::ResyncTarget::Buffers(v) if v.iter().any(|b| b == &self.buffer) => {
                if r.reason == wire::ResyncReason::Oversized
                    && let Some(rv) = r.rev
                    && matches!(self.phase, Phase::Live)
                {
                    if rv <= self.rev {
                        return;
                    }
                    // Our own op, replied rc 0 with exactly this rev, and every
                    // earlier event folded: ack without a snapshot (§3.4).
                    if let Some(Inflight::Local { present: true, state: AckState::Replied(info), .. }) = &self.inflight
                        && info.rev == rv
                        && self.rev + 1 == rv
                    {
                        self.rev = rv;
                        self.ack_inflight();
                        self.after_rev(step);
                        return;
                    }
                }
                step.extend(self.suspect());
            }
            _ => {}
        }
    }

    /// §3.4 steps 3–6 for an edit already past the duplicate and gap checks.
    fn fold_edit(&mut self, ev: &Ev<'_>, step: &mut Step) {
        debug_assert_eq!(ev.base_rev, self.rev, "folded past the gap check");
        if let Some(id) = ev.op_id {
            if let Some(Inflight::Local { p, present, state, .. }) = &self.inflight
                && p.op_id == id
            {
                if matches!(state, AckState::Doomed) {
                    // The server accepted an op the fold reverted: desync.
                    self.set_local_replied(ev.rev);
                    self.snapshot_fallback(true, step);
                    return;
                }
                if *present {
                    self.rev = ev.rev;
                    self.ack_inflight();
                } else {
                    // Not in the view (after a snapshot): its effect arrives now.
                    self.fold_remote(ev, step);
                    self.ack_inflight();
                }
                self.after_rev(step);
                return;
            }
            if self.completed.local.contains(id) {
                self.rev = ev.rev;
                self.after_rev(step);
                return;
            }
        }
        self.fold_remote(ev, step);
        self.after_rev(step);
    }

    /// §3.4 step 4: the transactional fold of a remote edit through the
    /// PRESENT pending ops, reverting a conflicting suffix and retrying.
    fn fold_remote(&mut self, ev: &Ev<'_>, step: &mut Step) {
        let mut conflict: Option<Conflict> = None;
        let mut desync = false;
        loop {
            let base = self.present_inflight() as usize;
            let mut work: Vec<Vec<Item>> = Vec::with_capacity(base + self.queue.len());
            if let Some(Inflight::Local { p, present: true, .. }) = &self.inflight {
                work.push(p.items.clone());
            }
            work.extend(self.queue.iter().map(|(_, p)| p.items.clone()));
            let mut out: Vec<Edit> = Vec::with_capacity(ev.edits.len());
            let mut failed: Option<usize> = None;
            'steps: for r in ev.edits {
                let mut x = r.offset..r.offset + r.delete;
                let ii = r.insert.len();
                for (j, items) in work.iter_mut().enumerate() {
                    let before: Vec<(Range<usize>, usize)> = items.iter().map(|it| (it.range.clone(), it.text.len())).collect();
                    let through = [(x.clone(), ii)];
                    for it in items.iter_mut() {
                        match ot::transform_through_set(it.range.clone(), &through, Priority::ThroughFirst) {
                            Ok(t) => it.range = t,
                            Err(_) => {
                                failed = Some(j);
                                break 'steps;
                            }
                        }
                    }
                    match ot::transform_through_set(x.clone(), &before, Priority::SelfFirst) {
                        Ok(t) => x = t,
                        Err(_) => {
                            failed = Some(j);
                            break 'steps;
                        }
                    }
                }
                out.push(Edit { offset: x.start, delete: x.end - x.start, insert: r.insert.clone() });
            }
            match failed {
                None => {
                    if apply_text(&mut self.text, &out).is_err() {
                        self.snapshot_fallback(true, step);
                        return;
                    }
                    let mut work = work.into_iter();
                    if let Some(Inflight::Local { p, present: true, .. }) = &mut self.inflight {
                        p.items = work.next().unwrap_or_default();
                    }
                    for (_, p) in self.queue.iter_mut() {
                        p.items = work.next().unwrap_or_default();
                    }
                    self.rev = ev.rev;
                    if let Some(c) = conflict.take() {
                        self.conflicts.push(c.clone());
                        step.notices.push(Notice::Conflict(c));
                    }
                    if ev.kind == wire::KindW::Reload {
                        self.meta.saved_rev = Some(ev.rev);
                    }
                    if !is_own_origin(ev.origin) {
                        let mut span: Option<Range<usize>> = None;
                        for e in &out {
                            if let Some(s) = span.as_mut() {
                                *s = map_point(s.start, Bias::Before, e).0..map_point(s.end, Bias::After, e).0;
                            }
                            if !e.insert.is_empty() {
                                let r = e.offset..e.offset + e.insert.len();
                                span = Some(match span {
                                    Some(s) => s.start.min(r.start)..s.end.max(r.end),
                                    None => r,
                                });
                            }
                        }
                        self.last_remote = Some(RemoteMark {
                            origin: ev.origin.to_string(),
                            lane: ev.lane.to_string(),
                            rev: ev.rev,
                            kind: ev.kind,
                            span,
                        });
                    }
                    let origin = ev.origin.parse().ok();
                    self.push_delta(step, out, origin, delta_kind(ev.kind));
                    if desync {
                        self.snapshot_fallback(true, step);
                    }
                    return;
                }
                Some(j) => {
                    desync |= self.revert_suffix(j, ev.rev, Some(ev.origin), &mut conflict, step);
                }
            }
        }
    }

    /// §3.7: revert ops `j..` of the present ops (in-flight first, then the
    /// queue) in reverse on the current text as one `Revert` delta; the
    /// in-flight op, if reverted, becomes `present = false, Doomed`. Returns
    /// true when that op had already been accepted (rc 0): a desync.
    fn revert_suffix(&mut self, j: usize, rev: u64, remote_origin: Option<&str>, acc: &mut Option<Conflict>, step: &mut Step) -> bool {
        let base = self.present_inflight() as usize;
        let removed: Vec<Pending> = self.queue.drain(j.saturating_sub(base)..).map(|(_, p)| p).collect();
        let mut edits = Vec::new();
        let mut lines = (usize::MAX, 0usize);
        for p in removed.iter().rev() {
            self.revert_op(p, &mut edits, &mut lines);
        }
        let mut texts = Vec::new();
        let mut desync = false;
        if j < base {
            let p = match &self.inflight {
                Some(Inflight::Local { p, .. }) => p.clone(),
                _ => unreachable!("present in-flight op"),
            };
            self.revert_op(&p, &mut edits, &mut lines);
            texts.extend(p.items.iter().filter(|i| !i.text.is_empty()).map(|i| i.text.clone()));
            if let Some(Inflight::Local { present, state, .. }) = &mut self.inflight {
                *present = false;
                match state {
                    AckState::Replied(_) => desync = true,
                    _ => *state = AckState::Doomed,
                }
            }
            if self.retry.as_ref().is_some_and(|r| r.waiting) {
                // Refused `busy`: never applied, so nothing to wait for.
                self.retry = None;
                self.clear_slot();
            }
        }
        for p in &removed {
            texts.extend(p.items.iter().filter(|i| !i.text.is_empty()).map(|i| i.text.clone()));
        }
        if !edits.is_empty() {
            self.push_delta(step, edits, None, DeltaKind::Revert);
        }
        let c = acc.get_or_insert_with(|| Conflict {
            rev,
            remote_origin: remote_origin.map(str::to_string),
            lines: (usize::MAX, 0),
            texts: Vec::new(),
        });
        c.lines = (c.lines.0.min(lines.0), c.lines.1.max(lines.1));
        if c.lines.0 == usize::MAX {
            c.lines = (1, 1);
        }
        c.texts.extend(texts);
        desync
    }

    fn revert_op(&mut self, p: &Pending, edits: &mut Vec<Edit>, lines: &mut (usize, usize)) {
        for e in p.inverse() {
            let a = self.text.point(e.offset).line;
            let b = self.text.point(e.offset + e.delete).line;
            *lines = (lines.0.min(a), lines.1.max(b));
            // Restoring text the op itself removed cannot exceed a limit the
            // view was already within.
            let _ = apply_text(&mut self.text, std::slice::from_ref(&e));
            edits.push(e);
        }
    }

    fn set_local_replied(&mut self, rev: u64) {
        if let Some(Inflight::Local { present, state, .. }) = &mut self.inflight {
            *present = false;
            *state = AckState::Replied(ReplyInfo { rev, truncated: false });
        }
    }

    /// The in-flight Local op is acknowledged: clear the slot.
    fn ack_inflight(&mut self) {
        if let Some(Inflight::Local { p, .. }) = self.inflight.take() {
            self.completed.add(p.op_id, true);
        }
        self.retry = None;
    }

    /// Clear the slot without acknowledging anything.
    fn clear_slot(&mut self) {
        if let Some(i) = self.inflight.take() {
            self.completed.add(i.wire().op_id.clone(), false);
        }
    }

    /// After `rev` moved: the Server completion barrier and the dirty flag.
    fn after_rev(&mut self, step: &mut Step) {
        self.check_barrier(step);
        self.meta.dirty = self.pending() > 0 || self.meta.saved_rev != Some(self.rev) || self.meta.recovered;
    }

    fn check_barrier(&mut self, step: &mut Step) {
        if let Some(Inflight::Server { state: AckState::Replied(info), .. }) = &self.inflight
            && self.rev >= info.rev
        {
            let rev = info.rev;
            self.complete_server(rev, step);
        }
    }

    fn complete_server(&mut self, rev: u64, step: &mut Step) {
        let Some(Inflight::Server { wire, .. }) = self.inflight.take() else { return };
        self.completed.add(wire.op_id.clone(), false);
        self.retry = None;
        let Some(k) = self.keep.as_mut() else { return };
        if k.current.as_deref() != Some(wire.op_id.as_str()) {
            return;
        }
        k.current = None;
        match k.compensating {
            Some(n) => {
                let n = n + 1;
                if n >= k.done.len() {
                    self.keep = None;
                    step.notices.push(Notice::Message {
                        level: Level::Warn,
                        text: "Keep mine was interrupted and undone; your copy is kept".into(),
                    });
                } else {
                    k.compensating = Some(n);
                    self.keep_next_undo(rev);
                }
            }
            None => {
                k.done.push((wire.op_id, rev));
                if k.stages.is_empty() {
                    self.keep = None;
                    self.detached_copy = None;
                } else {
                    self.keep_next_stage(rev);
                }
            }
        }
    }

    fn keep_next_stage(&mut self, expect_rev: u64) {
        let Some(k) = self.keep.as_mut() else { return };
        let Some(s) = k.stages.pop_front() else { return };
        k.current = Some(s.op_id.clone());
        let q = QServer {
            op: ServerOp::ApplyAt { items: vec![(s.range, s.text)], expect_rev },
            intent: k.intent.clone(),
            op_id: s.op_id,
            verb: Some(s.verb),
            expect: None,
        };
        self.queue_server(q);
    }

    /// Compensation: undo the newest committed stage not yet undone.
    fn keep_next_undo(&mut self, expect_rev: u64) {
        let Some(k) = self.keep.as_mut() else { return };
        let undone = k.compensating.unwrap_or(0);
        let Some((stage_id, _)) = k.done.iter().rev().nth(undone) else { return };
        let op_id = format!("{stage_id}.u");
        k.current = Some(op_id.clone());
        let q = QServer { op: ServerOp::Undo { lane: LaneArg::Own }, intent: k.intent.clone(), op_id, verb: None, expect: Some(expect_rev) };
        self.queue_server(q);
    }

    /// A keep-mine stage or compensating undo was refused.
    fn keep_refused(&mut self, message: &str, step: &mut Step) {
        let Some(k) = self.keep.as_mut() else { return };
        k.current = None;
        if k.compensating.is_some() {
            let steps = k.done.len();
            self.keep = None;
            step.notices.push(Notice::Message { level: Level::Error, text: format!("{MSG_KEEP_INTERRUPTED} (steps 1–{steps})") });
            return;
        }
        if k.done.is_empty() {
            self.keep = None;
            step.notices.push(Notice::Message { level: Level::Warn, text: format!("Keep mine was refused: {message}") });
            return;
        }
        k.compensating = Some(0);
        let rev = k.done.last().map(|(_, r)| *r).unwrap_or(self.rev);
        self.keep_next_undo(rev);
    }

    fn reply_ok(&mut self, v: &Value, step: &mut Step) {
        let rev = v.get("rev").and_then(Value::as_u64).unwrap_or(self.rev);
        let truncated = v.get("reply_truncated").and_then(Value::as_bool).unwrap_or(false);
        self.retry = None;
        match self.inflight.as_mut() {
            Some(Inflight::Local { state, .. }) => {
                if matches!(state, AckState::Doomed) {
                    self.set_local_replied(rev);
                    self.snapshot_fallback(true, step);
                    return;
                }
                *state = AckState::Replied(ReplyInfo { rev, truncated });
                // Folded before its reply (GLM F11), or contained in a snapshot.
                if rev <= self.rev {
                    self.ack_inflight();
                    self.after_rev(step);
                }
            }
            Some(Inflight::Server { op, state, .. }) => match op {
                ServerOp::Save { .. } => {
                    if let Ok(s) = serde_json::from_value::<wire::SaveReply>(v.clone()) {
                        self.meta.saved_rev = Some(s.saved_rev);
                        self.meta.path = Some(s.path);
                        self.meta.disk = s.disk;
                        self.meta.recovered = false;
                    }
                    self.complete_server(rev, step);
                    self.after_rev(step);
                }
                ServerOp::Reload { .. } if v.get("unchanged").and_then(Value::as_bool) == Some(true) => {
                    self.complete_server(rev, step);
                }
                _ => {
                    *state = AckState::Replied(ReplyInfo { rev, truncated });
                    self.check_barrier(step);
                }
            },
            None => {}
        }
    }

    fn reply_refused(&mut self, r: &wire::Refusal, step: &mut Step) {
        let why = r.reason.as_deref();
        if r.error_code == ErrorCode::NotFound && why == Some(reason::EPOCH_MISMATCH) {
            step.extend(self.epoch_changed());
            return;
        }
        let busy = r.error_code == ErrorCode::ResourceLimit && why == Some(reason::BUSY);
        match self.inflight.as_ref() {
            Some(Inflight::Local { present, state, .. }) => {
                if matches!(state, AckState::Doomed) || !*present {
                    // Expected: the op was already reverted and stashed.
                    self.clear_slot();
                    return;
                }
                if busy {
                    self.arm_retry();
                    return;
                }
                let mut acc = None;
                let rev = r.rev.unwrap_or(self.rev);
                self.revert_suffix(0, rev, None, &mut acc, step);
                self.clear_slot();
                if let Some(c) = acc {
                    self.conflicts.push(c.clone());
                    step.notices.push(Notice::Conflict(c));
                }
                if r.error_code != ErrorCode::Conflict {
                    step.notices.push(Notice::Message { level: Level::Error, text: r.message.clone() });
                }
                self.after_rev(step);
            }
            Some(Inflight::Server { op, wire, .. }) => {
                if busy {
                    self.arm_retry();
                    return;
                }
                let is_keep = self.keep.as_ref().is_some_and(|k| k.current.as_deref() == Some(wire.op_id.as_str()));
                let text = match op {
                    ServerOp::ApplyAt { .. } if why == Some(reason::STALE_REV) => {
                        "The text changed before the replace was applied; nothing was replaced".to_string()
                    }
                    ServerOp::Save { .. } if why == Some(reason::DISK_MODIFIED) => MSG_SAVE_DISK_MODIFIED.to_string(),
                    _ => r.message.clone(),
                };
                self.clear_slot();
                if is_keep {
                    self.keep_refused(&text, step);
                } else {
                    step.notices.push(Notice::Message { level: Level::Warn, text });
                }
            }
            None => {}
        }
    }

    fn arm_retry(&mut self) {
        let r = self.retry.get_or_insert(Retry { next_delay: RETRY_FIRST_MS, unarmed: None, waiting: false });
        r.unarmed = Some(r.next_delay);
        r.waiting = true;
        r.next_delay = (r.next_delay * 2).min(RETRY_CAP_MS);
    }

    fn start_recon(&mut self, step: &mut Step) {
        let Some(inflight) = &self.inflight else { return };
        let w = inflight.wire();
        self.recon = Some(Recon {
            op_id: w.op_id.clone(),
            since: w.sent_at_rev,
            found: None,
            trimmed: false,
            first: true,
            entries: Vec::new(),
            save_resent: false,
        });
        let body = json!({"buffer": self.buffer, "since_rev": w.sent_at_rev, "limit": HISTORY_LIMIT});
        self.send_read(Read::Recon, "edit.history", body, DEADLINE_MS, step);
    }

    fn on_read_reply(&mut self, kind: Read, reply: Result<Value, wire::Refusal>) -> Step {
        let mut step = Step::default();
        let v = match reply {
            Ok(v) => v,
            Err(r) => {
                self.read_refused(kind, &r, &mut step);
                return step;
            }
        };
        match kind {
            Read::Page => match serde_json::from_value::<wire::GetReply>(v) {
                Ok(page) => step = self.on_page(&page),
                Err(e) => self.detach_failed(format!("bad edit.get reply: {e}")),
            },
            Read::Recover | Read::Recon => match serde_json::from_value::<wire::HistoryReply>(v) {
                Ok(page) => self.history_page(kind, &page, &mut step),
                Err(_) => self.snapshot_fallback(false, &mut step),
            },
            Read::SaveCheck => {
                let list = serde_json::from_value::<wire::ListReply>(v).ok();
                self.save_check(list, &mut step);
            }
        }
        self.try_snapshot(&mut step);
        step
    }

    fn read_refused(&mut self, kind: Read, r: &wire::Refusal, step: &mut Step) {
        let why = r.reason.as_deref();
        if r.error_code == ErrorCode::NotFound && why == Some(reason::EPOCH_MISMATCH) {
            step.extend(self.epoch_changed());
            return;
        }
        if r.error_code == ErrorCode::NotFound && why == Some(reason::UNKNOWN_BUFFER) {
            self.phase = Phase::Detached { reason: DetachReason::ClosedRemotely { by: None } };
            self.abandon_pipeline();
            return;
        }
        match kind {
            Read::Page if why == Some(reason::SNAPSHOT_EXPIRED) => self.start_pages(step),
            Read::Page => self.detach_failed(r.message.clone()),
            Read::Recover => self.snapshot_fallback(false, step),
            Read::Recon => {
                // Unanswerable: treat as a trimmed log (never a blind resend).
                if let Some(rc) = self.recon.as_mut() {
                    rc.trimmed = true;
                }
                self.finish_recon(step);
            }
            Read::SaveCheck => self.save_check(None, step),
        }
        self.try_snapshot(step);
    }

    fn detach_failed(&mut self, msg: String) {
        self.phase = Phase::Detached { reason: DetachReason::OpenFailed { msg } };
        self.abandon_pipeline();
    }

    fn history_page(&mut self, kind: Read, page: &wire::HistoryReply, step: &mut Step) {
        if page.buffer != self.buffer {
            return;
        }
        match kind {
            Read::Recover => {
                if !matches!(self.phase, Phase::Recovering { how: RecoverHow::History, .. }) {
                    return;
                }
                if page.oldest_rev > self.rev && page.rev > self.rev {
                    self.snapshot_fallback(false, step);
                    return;
                }
                if !self.fold_entries(&page.entries, step) {
                    if matches!(self.phase, Phase::Recovering { how: RecoverHow::History, .. }) {
                        self.snapshot_fallback(false, step);
                    }
                    return;
                }
                if !matches!(self.phase, Phase::Recovering { how: RecoverHow::History, .. }) {
                    return;
                }
                if page.truncated
                    && let Some(next) = page.next
                {
                    let body = json!({"buffer": self.buffer, "since_rev": next, "limit": HISTORY_LIMIT});
                    self.send_read(Read::Recover, "edit.history", body, DEADLINE_MS, step);
                    return;
                }
                self.go_live(step);
            }
            Read::Recon => {
                let Some(rc) = self.recon.as_mut() else { return };
                if rc.first {
                    rc.first = false;
                    rc.trimmed = page.oldest_rev > rc.since;
                }
                for e in &page.entries {
                    if e.op_id.as_deref() == Some(rc.op_id.as_str()) {
                        rc.found = Some(e.rev);
                    }
                }
                rc.entries.extend(page.entries.iter().cloned());
                if page.truncated
                    && let Some(next) = page.next
                {
                    let body = json!({"buffer": self.buffer, "since_rev": next, "limit": HISTORY_LIMIT});
                    self.send_read(Read::Recon, "edit.history", body, DEADLINE_MS, step);
                    return;
                }
                self.finish_recon(step);
            }
            _ => {}
        }
    }

    /// Fold history entries past `rev` as events (`base_rev = rev - 1`).
    /// `false` when an entry is elided or does not chain — snapshot needed.
    fn fold_entries(&mut self, entries: &[wire::HistoryEntryW], step: &mut Step) -> bool {
        for e in entries {
            if e.rev <= self.rev {
                continue;
            }
            let Some(edits) = e.edits.as_ref().filter(|_| !e.edits_elided) else { return false };
            if e.rev != self.rev + 1 {
                return false;
            }
            let ev = Ev {
                rev: e.rev,
                base_rev: e.rev - 1,
                origin: &e.origin,
                lane: &e.lane,
                kind: e.kind,
                op_id: e.op_id.as_deref(),
                edits,
            };
            self.fold_edit(&ev, step);
            if self.snap_due.is_some() || self.pager.is_some() || matches!(self.phase, Phase::Detached { .. }) {
                return true;
            }
        }
        true
    }

    /// §3.5: act on a finished reconciliation read.
    fn finish_recon(&mut self, step: &mut Step) {
        let Some(rc) = self.recon.take() else { return };
        if self.inflight.as_ref().map(|i| i.wire().op_id.as_str()) != Some(rc.op_id.as_str()) {
            return; // resolved meanwhile
        }
        let live = matches!(self.phase, Phase::Live);
        match self.inflight.as_mut() {
            Some(Inflight::Local { state, wire, .. }) => match (rc.found, rc.trimmed) {
                (Some(r), _) => {
                    if matches!(state, AckState::Doomed) {
                        self.set_local_replied(r);
                        self.snapshot_fallback(true, step);
                        return;
                    }
                    *state = AckState::Replied(ReplyInfo { rev: r, truncated: false });
                    if r <= self.rev {
                        self.ack_inflight();
                        self.after_rev(step);
                    } else if live && !self.fold_entries(&rc.entries, step) {
                        self.snapshot_fallback(false, step);
                    }
                }
                (None, false) => {
                    if matches!(state, AckState::Doomed) {
                        self.clear_slot();
                    } else {
                        // Not committed and the log covers the window: resend
                        // the identical request (same bytes, same op_id).
                        *state = AckState::Sent;
                        step.out.push(Self::resend(wire));
                    }
                }
                (None, true) => {
                    let doomed = matches!(state, AckState::Doomed);
                    self.inflight = None;
                    if !doomed {
                        // Unknown: keep the view as a detached copy and resync.
                        self.snapshot_fallback(true, step);
                    }
                }
            },
            Some(Inflight::Server { op, state, .. }) => {
                let msg = match op {
                    ServerOp::Undo { .. } => MSG_UNDO_INCOMPLETE,
                    ServerOp::Redo { .. } => MSG_REDO_INCOMPLETE,
                    _ => MSG_REPLACE_INCOMPLETE,
                };
                match (rc.found, rc.trimmed) {
                    (Some(r), _) => {
                        *state = AckState::Replied(ReplyInfo { rev: r, truncated: false });
                        if r > self.rev && live && !self.fold_entries(&rc.entries, step) {
                            self.snapshot_fallback(false, step);
                        }
                        self.check_barrier(step);
                    }
                    (None, trimmed) => {
                        let is_keep = self.keep.as_ref().is_some_and(|k| k.current.as_deref() == Some(rc.op_id.as_str()));
                        self.clear_slot();
                        if is_keep {
                            self.keep_refused(msg, step);
                        } else {
                            step.notices.push(Notice::Message { level: Level::Warn, text: msg.into() });
                        }
                        if trimmed {
                            step.extend(self.suspect());
                        }
                    }
                }
            }
            None => {}
        }
    }

    fn save_check(&mut self, list: Option<wire::ListReply>, step: &mut Step) {
        let Some(mut rc) = self.recon.take() else { return };
        let Some(Inflight::Server { op: ServerOp::Save { .. }, wire, state, .. }) = self.inflight.as_mut() else { return };
        if wire.op_id != rc.op_id {
            return;
        }
        let row = list.as_ref().and_then(|l| l.buffers.iter().find(|b| b.buffer == self.buffer));
        if let Some(row) = row
            && row.saved_rev.is_some_and(|s| s >= wire.sent_at_rev)
        {
            self.meta.saved_rev = row.saved_rev;
            self.meta.path = row.path.clone();
            self.meta.disk = row.disk;
            self.meta.recovered = row.recovered;
            let rev = self.rev;
            self.complete_server(rev, step);
            self.after_rev(step);
            return;
        }
        if !rc.save_resent {
            rc.save_resent = true;
            *state = AckState::Sent;
            step.out.push(Self::resend(wire));
            self.recon = Some(rc);
            return;
        }
        self.clear_slot();
        step.notices.push(Notice::Message { level: Level::Warn, text: MSG_SAVE_UNCERTAIN.into() });
    }

    /// Start (or schedule) the snapshot fallback of §3.6.
    fn snapshot_fallback(&mut self, detach: bool, step: &mut Step) {
        if detach && self.detached_copy.is_none() {
            let text = read_all(&self.text);
            step.notices.push(Notice::DetachedCopy { bytes: text.len() });
            self.detached_copy = Some(DetachedCopy { text, rev_seen: self.rev });
        }
        let buffered = match std::mem::replace(&mut self.phase, Phase::Live) {
            Phase::Bootstrapping { buffered } | Phase::Recovering { buffered, .. } => buffered,
            other => {
                if matches!(other, Phase::Detached { .. }) {
                    self.phase = other;
                    return;
                }
                Vec::new()
            }
        };
        self.phase = Phase::Recovering { since: self.rev, buffered, how: RecoverHow::Snapshot };
        self.reads.retain(|_, (k, _)| *k != Read::Recover);
        self.snap_due = Some(self.snap_due.unwrap_or(false) || detach);
        self.try_snapshot(step);
    }

    /// Proceed with a due snapshot once the in-flight op is resolved.
    fn try_snapshot(&mut self, step: &mut Step) {
        if self.snap_due.is_none() || self.pager.is_some() {
            return;
        }
        if let Some(Inflight::Local { state, .. }) = &self.inflight {
            match state {
                AckState::Sent | AckState::Doomed => return,
                AckState::Uncertain => {
                    if self.recon.is_none() {
                        self.start_recon(step);
                    }
                    return;
                }
                AckState::Replied(_) => {}
            }
        }
        // Queued (unsent) ops certainly did not commit: revert and stash.
        if !self.queue.is_empty() {
            let base = self.present_inflight() as usize;
            let mut acc = None;
            let rev = self.rev;
            self.revert_suffix(base, rev, None, &mut acc, step);
            if let Some(c) = acc {
                self.conflicts.push(c.clone());
                step.notices.push(Notice::Conflict(c));
            }
        }
        self.snap_due = None;
        self.start_pages(step);
    }

    /// The final snapshot page arrived: replace the text, resolve the
    /// in-flight op, replay buffered events, go Live.
    fn finish_snapshot(&mut self, text: String, rev: u64, step: &mut Step) {
        let replace = match self.compare_with.take() {
            Some(old) => {
                if blake3::hash(old.as_bytes()) == blake3::hash(text.as_bytes()) {
                    false
                } else {
                    step.notices.push(Notice::DetachedCopy { bytes: old.len() });
                    self.detached_copy = Some(DetachedCopy { text: old, rev_seen: rev });
                    true
                }
            }
            None => true,
        };
        if replace {
            match Text::from_text(&text) {
                Ok(t) => self.text = t,
                Err(e) => {
                    self.detach_failed(e.to_string());
                    return;
                }
            }
        }
        self.rev = rev;
        self.cursors.clear();
        if replace {
            self.push_delta(step, Vec::new(), None, DeltaKind::Resync);
        }
        let contained = matches!(&self.inflight,
            Some(Inflight::Local { state: AckState::Replied(info), .. }) if info.rev <= rev);
        if contained {
            self.ack_inflight();
        } else if let Some(Inflight::Local { present, .. }) = &mut self.inflight {
            // Past the snapshot: its echo folds as remote and acks it.
            *present = false;
        }
        self.go_live(step);
    }

    /// Leave Bootstrapping / Recovering: replay buffered events past `rev`.
    fn go_live(&mut self, step: &mut Step) {
        let buffered = match std::mem::replace(&mut self.phase, Phase::Live) {
            Phase::Bootstrapping { buffered } | Phase::Recovering { buffered, .. } => buffered,
            other => {
                self.phase = other;
                return;
            }
        };
        self.after_rev(step);
        for ev in buffered {
            if matches!(self.phase, Phase::Live) {
                self.on_edit_event(&ev, step);
            } else if let Phase::Recovering { buffered, .. } | Phase::Bootstrapping { buffered } = &mut self.phase {
                buffered.push(ev);
            } else {
                break;
            }
        }
        if matches!(self.phase, Phase::Live) && std::mem::take(&mut self.resuspect) {
            step.extend(self.suspect());
        }
    }
}

fn op_json(r: &Range<usize>, t: &str) -> Value {
    if r.is_empty() {
        json!({"op": "insert", "at": r.start, "text": t})
    } else if t.is_empty() {
        json!({"op": "delete", "range": [r.start, r.end]})
    } else {
        json!({"op": "replace", "range": [r.start, r.end], "text": t})
    }
}
