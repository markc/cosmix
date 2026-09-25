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

use std::collections::VecDeque;
use std::ops::Range;

use cosmix_edit_core::ot::Edit;
use cosmix_edit_core::text::Text;
use cosmix_edit_core::wire;

use crate::types::{Conflict, Intent, LocalEdit, Notice, OpIdGen, Outgoing, ViewDelta};

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
        todo!("ced E1d")
    }

    /// Inverse steps: for the steps in REVERSE application order,
    /// `(offset, text.len(), item.deleted)`.
    pub fn inverse(&self) -> Vec<Edit> {
        todo!("ced E1d")
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorError {
    /// Not `Live` (bootstrapping, recovering or detached).
    NotLive,
    /// More than `MAX_OPS_PER_TXN` items or 1 MiB inserted (plan §3.2).
    TooLarge { items: usize, bytes: usize },
    /// Items overlap, are reversed, or leave a char boundary.
    Invalid(String),
}

/// See the module docs.
pub struct Mirror {
    buffer: wire::BufferId,
    epoch: String,
    text: Text,
    rev: u64,
    view_gen: u64,
    inflight: Option<Inflight>,
    queue: VecDeque<Pending>,
    server_ops: VecDeque<(ServerOp, Intent)>,
    conflicts: Vec<Conflict>,
    detached_copy: Option<DetachedCopy>,
    meta: BufferMeta,
    last_remote: Option<RemoteMark>,
    phase: Phase,
}

impl Mirror {
    /// From an `edit.open` reply: `Bootstrapping`, and the first
    /// `edit.get {snapshot:true}` to send.
    pub fn bootstrap(open: &wire::OpenReply) -> Result<(Mirror, Step), MirrorError> {
        let _ = open;
        todo!("ced E1d")
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

    /// Apply an optimistic local edit now (one `Local` delta) and queue it.
    pub fn local_edit(&mut self, e: LocalEdit, intent: Intent, ids: &mut OpIdGen) -> Result<Step, MirrorError> {
        let _ = (e, intent, ids);
        todo!("ced E1d")
    }

    /// Queue a non-optimistic request (sent when the pipeline is empty).
    pub fn server_op(&mut self, op: ServerOp, intent: Intent, ids: &mut OpIdGen) -> Step {
        let _ = (op, intent, ids);
        todo!("ced E1d")
    }

    /// An `edit.changed` event for THIS buffer (the controller filters by
    /// buffer and tracks the global `event_seq`).
    pub fn on_event(&mut self, ev: &wire::Event) -> Step {
        let _ = ev;
        todo!("ced E1d")
    }

    /// The reply to one of this mirror's requests, matched by op_id (or by the
    /// request the controller correlated, for reads).
    pub fn on_reply(&mut self, op_id: &str, reply: Result<serde_json::Value, wire::Refusal>) -> Step {
        let _ = (op_id, reply);
        todo!("ced E1d")
    }

    /// The request carrying `op_id` passed its deadline with no reply (§3.5).
    pub fn on_deadline(&mut self, op_id: &str) -> Step {
        let _ = op_id;
        todo!("ced E1d")
    }

    /// One page of `edit.get snapshot:true` (bootstrap or snapshot recovery).
    pub fn on_page(&mut self, page: &wire::GetReply) -> Step {
        let _ = page;
        todo!("ced E1d")
    }

    /// One page of `edit.history` (recovery or deadline reconciliation).
    pub fn on_history(&mut self, page: &wire::HistoryReply) -> Step {
        let _ = page;
        todo!("ced E1d")
    }

    /// Possible loss: recover from history (§3.6).
    pub fn suspect(&mut self) -> Step {
        todo!("ced E1d")
    }

    /// The scheduler (§3.3): the next request to send, if any.
    pub fn next_outgoing(&mut self) -> Option<Outgoing> {
        todo!("ced E1d")
    }
}
