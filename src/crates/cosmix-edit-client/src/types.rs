//! Shared frontend types (ced E1 plan §1.4, §3.1) — **complete and frozen in
//! Stage S**. Every frontend (the ced app, the E2 scene widget, headless
//! harnesses) speaks these.

use std::ops::Range;

use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::origin::{Origin, OriginKind};
use cosmix_edit_core::ot::Edit;

/// A ced tab (one per buffer view). Opaque, process-unique.
pub type TabId = u64;

/// The origin every window-input edit claims: keyboard, IME, mouse, drops —
/// including compositor-injected input, which ced cannot tell from a physical
/// keyboard. It labels the UI-input *route*; it attests nothing (plan D5).
pub const UI_ORIGIN: &str = "human:ced";

/// A buffer on a particular daemon session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BufferRef {
    pub buffer: String,
    pub epoch: String,
}

/// Generates `op_id`s `c<8 hex run id>-<seq, zero-padded to ≥6>` — random per
/// process start, monotonic within it, so ids never repeat across restarts
/// and always match editd's `^[A-Za-z0-9._:-]{1,64}$`.
#[derive(Debug, Clone)]
pub struct OpIdGen {
    run: u32,
    seq: u64,
}

impl OpIdGen {
    /// `run` must be random per process start (the caller supplies it so this
    /// crate needs no RNG).
    pub fn new(run: u32) -> Self {
        Self { run, seq: 0 }
    }

    pub fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("c{:08x}-{:06}", self.run, self.seq)
    }

    /// A tagged id for a multi-step operation (e.g. keep-mine: `keep<k>-<i>`).
    pub fn next_tagged(&mut self, tag: &str) -> String {
        self.seq += 1;
        format!("c{:08x}-{tag}-{:06}", self.run, self.seq)
    }
}

/// Who asked for an operation — carried through every asynchronous completion
/// (paste, dialogs, find/replace) so provenance never changes mid-flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invoker {
    /// ced's own window input.
    Ui,
    /// A Bus caller, by its attested caller key (`local:<from>`,
    /// `mesh:<service>@<peer>`, `anon`).
    Bus { caller_key: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub origin: Origin,
    pub tab: TabId,
    pub by: Invoker,
}

impl Intent {
    /// The intent of ced's own window input on `tab`.
    pub fn ui(tab: TabId) -> Self {
        Self { origin: UI_ORIGIN.parse().expect("valid origin"), tab, by: Invoker::Ui }
    }

    /// The intent of a Bus caller on `tab`: origin `agent:<bus_lane_label>`.
    pub fn bus(tab: TabId, caller_key: &str) -> Self {
        Self {
            origin: Origin::new(OriginKind::Agent, bus_lane_label(caller_key)),
            tab,
            by: Invoker::Bus { caller_key: caller_key.to_string() },
        }
    }
}

/// The lane label for a Bus caller driving ced (plan D5, GLM round-2 #1): the
/// WHOLE label must fit editd's 64-char grammar `^[A-Za-z0-9._@/+-]{1,64}$`.
/// `"ced." + caller` when that fits (characters outside the grammar become
/// `_`); otherwise `"ced." + first 43 chars + "+" + 16 lowercase hex of
/// blake3(full caller key)` — exactly 64.
pub fn bus_lane_label(caller_key: &str) -> String {
    let clean: String = caller_key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._@/+-".contains(c) { c } else { '_' })
        .collect();
    let full = format!("ced.{clean}");
    if !clean.is_empty() && full.len() <= 64 {
        return full;
    }
    let hash = blake3::hash(caller_key.as_bytes()).to_hex();
    let head: String = clean.chars().take(43).collect();
    format!("ced.{head}+{}", &hash[..16])
}

/// A local (optimistic) edit produced by the editor model: a base-coordinate
/// transaction on the CURRENT view text, items in request order,
/// non-overlapping (one item for ordinary typing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEdit {
    pub items: Vec<(Range<usize>, String)>,
    /// Single-grapheme typing and backspace/delete runs only.
    pub coalesce: bool,
    /// The editing view's selection after the edit, in post-edit coordinates.
    pub caret_after: Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Our own optimistic edit, applied now.
    Local,
    /// Another origin's edit (or our own non-optimistic server op).
    Remote,
    Undo,
    Redo,
    Reload,
    /// Our pending ops undone locally (a conflict or refusal).
    Revert,
    /// The whole text was replaced (snapshot); no edits — clamp and invalidate.
    Resync,
}

/// The single thing the editor side consumes: how the view text changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDelta {
    /// VIEW coordinates, application order (empty for `Resync`).
    pub edits: Vec<Edit>,
    pub origin: Option<Origin>,
    pub kind: DeltaKind,
    /// The mirror's server rev after this delta.
    pub rev: u64,
    /// The mirror's view generation after this delta (+1 per delta).
    pub view_gen: u64,
}

/// A request for the transport to send (JSON body; the transport adds the
/// correlation and deadline). `op_id` repeats the body's for correlation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub verb: String,
    pub body: String,
    pub op_id: Option<String>,
    /// Transport deadline in milliseconds (plan §2: 5 s, 30 s for open/get/save).
    pub deadline_ms: u64,
}

/// What the transport delivers back to a controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    /// A Bus reply to a request the controller correlated as `req`.
    Reply { req: u64, rc: u8, body: String },
    /// A topic delivery (`edit.changed`, `theme.changed`, …): raw inner body.
    Topic { topic: String, body: String },
    /// A one-shot timer the controller armed fired.
    Timer { id: u64 },
    /// The request `req` passed its deadline with no reply.
    Deadline { req: u64 },
    /// The Bus connection went down (`false`) or came back (`true`).
    Connection { up: bool },
}

/// Local text that could not be applied because another origin edited the
/// same span first (plan §3.7). One per remote event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The server rev of the remote edit that won.
    pub rev: u64,
    pub remote_origin: Option<String>,
    /// 1-based inclusive line span of the reverted ops (view, before revert).
    pub lines: (usize, usize),
    /// The inserted texts of the reverted ops, in the order they were typed.
    pub texts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Something the chrome should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Conflict(Conflict),
    /// The view text was kept as a detached copy (epoch change or unknown op).
    DetachedCopy { bytes: usize },
    /// Free-form status (e.g. "Undo did not complete — press again").
    Message { level: Level, text: String },
}
