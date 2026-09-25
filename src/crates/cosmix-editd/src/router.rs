//! The router: global verbs, buffer table, and every cross-buffer invariant
//! (plan §4.8, D10).
//!
//! # Contract: refusal precedence (frozen; plan §4.1)
//! The first failing check wins:
//! 1. unknown verb → UNKNOWN_VERB;
//! 2. argument shape (serde into the `wire` request type; both `expect_rev`
//!    and `base_rev` → `both_cas`) → INVALID_ARGUMENT;
//! 3. mutating verb without a `broker_origin` stamp → INVALID_ARGUMENT `unstamped`;
//! 4. mesh lock (`COSMIX_MESH_OPEN=0`, mutating verb, `broker_origin != local`) → FORBIDDEN `mesh_locked`;
//! 5. buffer lookup: wrong epoch → NOT_FOUND `epoch_mismatch`; unknown → `unknown_buffer`;
//! 6. inbox full → RESOURCE_LIMIT `busy`;
//! 7. (in the actor) op_id duplicate → the cached original reply, `duplicate: true`;
//! 8. CAS (`stale_rev`, `history_trimmed`, …);
//! 9. position resolution / validation;
//! 10. limits and byte budget;
//! 11. I/O.
//!
//! # Contract: path table (frozen)
//! The router processes its inbox strictly in order and is the ONLY writer of
//! [`PathSlot`]s. Actors ask over a oneshot; nothing is a lock held across I/O.
//! - `edit.open P`: `Bound` → reopen (add holder); `Loading`/`Closing` → park
//!   in `waiters`; absent → insert `Loading`, take a byte lease for the stat'd
//!   size, spawn the actor, whose FIRST job is the load (`spawn_blocking`).
//!   `Loaded` → `Bound` and every waiter gets the same buffer; `Failed(refusal)`
//!   → slot removed, lease released, every waiter gets the same refusal.
//!   `Closing` waiters re-run as fresh opens once the close completes.
//!   Parked waiters join the initiator's open: ITS `create` / `language`
//!   decide the buffer (a parked `create:false` opener may receive a buffer
//!   the initiator created).
//! - save-as `P`: `ReserveSaveAs(P)`: absent → `SaveAs`, anything else →
//!   CONFLICT `path_open`. After the rename `CommitSaveAs(old, P)` removes
//!   `old` whatever its state, binds `P` — as `Closing` if a close is in
//!   flight, so that close now concerns `P` — moves the watch, and re-runs any
//!   opens parked on `old` as fresh opens; one inbox step. Any failure →
//!   `ReleaseSaveAs(P)`. An open of `P` while `SaveAs` → CONFLICT `path_open`.
//! - `edit.close`: `Closing`, forward to the actor; the actor stops, releases
//!   its lease, reports `Closed`; the slot is removed and waiters served.
//! - Scratch buffers have no slot. `MAX_BUFFERS` is checked/incremented when a
//!   slot or scratch buffer is created, decremented on `Closed`.
//!
//! # Shape (E0b)
//! [`Editd::submit`] is the front door. It runs checks 1-6 synchronously, in
//! command receive order, and hands the command to its owner with `try_send`
//! — so per-buffer order is the order commands were submitted, and nothing on
//! the command path ever blocks. Global verbs (`open`, `close`, `list`,
//! `info`, `props.*`) go to the router task's bounded inbox; buffer verbs go
//! straight to the owning actor's bounded inbox (the actor table is written
//! only by the router). Actors talk back over an unbounded internal channel
//! that the router drains ahead of new commands. `Closed` is reported by a
//! monitor task when the actor task has really ended (its lease dropped),
//! whether it stopped or panicked.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use cosmix_client::IncomingCommand;
use cosmix_edit_core::anchor::is_valid_name;
use cosmix_edit_core::error::{ErrorCode, reason};
use cosmix_edit_core::wire::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{ActorInit, ActorMsg, BufVerb, Init};
use crate::caller::{self, Caller};
use crate::events::{EventSink, Publisher};
use crate::limits::{
    ACTOR_INBOX, LANGUAGE_MAX, MAX_BUFFERS, MAX_EVENT_BYTES, MAX_HOLDERS, MAX_REPLY_BYTES, MAX_TOTAL_BYTES, ROUTER_INBOX,
};
use crate::props::{BufferProps, EditProps, buffer_leaves};
use crate::refusal::{RefusalExt, bad_args, busy, refusal, render, router_busy, unknown_buffer};
use crate::watch::{DiskSignal, Watch};

/// `(rc, body)` of one Bus response.
pub type Reply = (u8, String);

/// A parked or in-flight `edit.open`, re-runnable as a fresh open.
pub struct OpenWaiter {
    pub caller: String,
    pub canonical: Option<PathBuf>,
    pub opened_as: Option<String>,
    pub size: Option<u64>,
    pub create: bool,
    pub language: Option<String>,
    pub reply: oneshot::Sender<Reply>,
}

pub enum PathSlot {
    Loading { bid: BufferId, waiters: Vec<OpenWaiter> },
    Bound { bid: BufferId },
    SaveAs { bid: BufferId },
    Closing { bid: BufferId, waiters: Vec<OpenWaiter> },
}

impl PathSlot {
    fn bid(&self) -> &BufferId {
        match self {
            PathSlot::Loading { bid, .. }
            | PathSlot::Bound { bid }
            | PathSlot::SaveAs { bid }
            | PathSlot::Closing { bid, .. } => bid,
        }
    }
}

/// Canonical path → slot. Router-owned.
pub type PathTable = HashMap<PathBuf, PathSlot>;

/// The aggregate byte budget (`MAX_TOTAL_BYTES`: text + retained log text +
/// snapshots). Created by the router, shared with actors. `try_lease` is an
/// atomic `fetch_update` with a checked add, so two actors can never both
/// pass a check only one fits.
///
/// Leases are taken in phase 1 of a transaction (peak growth of text + the log
/// text the entry adds), at open (file size) and at snapshot creation; released
/// on shrinkage (`peak - final` after phase 2), log trimming, snapshot release
/// and actor exit. A refused lease → RESOURCE_LIMIT `budget`, nothing changed.
#[derive(Debug)]
pub struct Budget {
    cap: u64,
    used: AtomicU64,
}

impl Budget {
    pub fn new(cap: u64) -> Self {
        Self { cap, used: AtomicU64::new(0) }
    }

    /// Reserve `n` bytes, or `false` with nothing reserved.
    pub fn try_lease(&self, n: u64) -> bool {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(n).filter(|total| *total <= self.cap)
            })
            .is_ok()
    }

    /// Return `n` previously leased bytes.
    pub fn release(&self, n: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| Some(used.saturating_sub(n)));
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
}

/// Messages actors send the router (answers come back on the oneshot).
pub enum ToRouter {
    /// First message of a successful actor: its coarse state.
    Loaded { bid: BufferId, props: BufferProps, created: bool },
    Failed { bid: BufferId, refusal: Refusal },
    /// Coarse state changed.
    State { bid: BufferId, props: BufferProps },
    ReserveSaveAs { bid: BufferId, path: PathBuf, reply: oneshot::Sender<Result<(), Refusal>> },
    CommitSaveAs { bid: BufferId, old: Option<PathBuf>, new: PathBuf, reply: oneshot::Sender<()> },
    ReleaseSaveAs { path: PathBuf },
    /// The actor's answer to a forwarded close; `Ok` means it is stopping.
    CloseDecided { bid: BufferId, result: Result<(), Refusal>, reply: oneshot::Sender<Reply> },
    /// The actor task ended (stopped, failed or panicked): its lease is gone.
    Closed { bid: BufferId },
}

/// Commands for the router's bounded inbox.
pub enum RouterCmd {
    Open(OpenWaiter),
    Close { bid: BufferId, force: bool, caller: String, reply: oneshot::Sender<Reply> },
    Info { reply: oneshot::Sender<Reply> },
    List { reply: oneshot::Sender<Reply> },
    Props { suffix: String, args: Option<Value>, reply: oneshot::Sender<Reply> },
    Dirty { reply: oneshot::Sender<Vec<DirtyBuffer>> },
}

/// A buffer with unsaved text (the SIGTERM log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyBuffer {
    pub buffer: BufferId,
    pub path: Option<String>,
    pub rev: u64,
}

struct Entry {
    path: Option<PathBuf>,
    holders: Vec<String>,
    props: Option<BufferProps>,
    signal: Arc<DiskSignal>,
    tx: mpsc::Sender<ActorMsg>,
    closing: bool,
    /// Scratch opens waiting for the actor's `Loaded`.
    pending: Vec<OpenWaiter>,
}

type ActorTable = Arc<RwLock<HashMap<BufferId, mpsc::Sender<ActorMsg>>>>;

/// Router state. Owned by the router task.
pub struct Router {
    pub epoch: String,
    pub paths: PathTable,
    pub budget: Arc<Budget>,
    pub buffer_count: usize,
    pub next_buffer: u64,
    pub mesh_open: bool,
    entries: BTreeMap<BufferId, Entry>,
    actors: ActorTable,
    publisher: Arc<Publisher>,
    watch: Watch,
    internal_tx: mpsc::UnboundedSender<ToRouter>,
    snapshot_seq: Arc<AtomicU64>,
}

impl Router {
    /// `b<N>_<epoch>`.
    pub fn buffer_id(&mut self) -> BufferId {
        self.next_buffer += 1;
        format!("b{}_{}", self.next_buffer, self.epoch)
    }
}

fn send(reply: oneshot::Sender<Reply>, r: Result<String, Refusal>) {
    let _ = reply.send(match r {
        Ok(body) => (0, body),
        Err(refusal) => render(&refusal),
    });
}

fn json_of<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

impl Router {
    async fn run(mut self, mut cmds: mpsc::Receiver<RouterCmd>, mut internal: mpsc::UnboundedReceiver<ToRouter>) {
        loop {
            tokio::select! {
                biased;
                Some(msg) = internal.recv() => self.internal(msg),
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => break,
                },
            }
        }
    }

    fn command(&mut self, cmd: RouterCmd) {
        match cmd {
            RouterCmd::Open(w) => self.open(w),
            RouterCmd::Close { bid, force, caller, reply } => self.close(bid, force, caller, reply),
            RouterCmd::Info { reply } => send(reply, Ok(self.info())),
            RouterCmd::List { reply } => send(reply, Ok(self.list())),
            RouterCmd::Props { suffix, args, reply } => {
                let tree = self.props_tree();
                let response = cosmix_props_core::bus::dispatch_props(&tree, &suffix, args.as_ref(), true);
                let _ = reply.send((response.rc.clamp(0, 255) as u8, response.body));
            }
            RouterCmd::Dirty { reply } => {
                let dirty = self
                    .entries
                    .iter()
                    .filter_map(|(bid, e)| {
                        let p = e.props.as_ref()?;
                        p.dirty.then(|| DirtyBuffer { buffer: bid.clone(), path: p.path.clone(), rev: p.rev })
                    })
                    .collect();
                let _ = reply.send(dirty);
            }
        }
    }

    fn internal(&mut self, msg: ToRouter) {
        match msg {
            ToRouter::Loaded { bid, props, created } => self.loaded(bid, props, created),
            ToRouter::Failed { bid, refusal } => self.failed(bid, refusal),
            ToRouter::State { bid, props } => {
                let Some(entry) = self.entries.get(&bid) else { return };
                let old = entry.props.clone();
                let holders = entry.holders.clone();
                self.publish_props(&bid, old.as_ref().map(|p| (p, holders.as_slice())), Some((&props, holders.as_slice())));
                if let Some(entry) = self.entries.get_mut(&bid) {
                    entry.props = Some(props);
                }
            }
            ToRouter::ReserveSaveAs { bid, path, reply } => {
                let result = match self.paths.get(&path) {
                    None => {
                        self.paths.insert(path, PathSlot::SaveAs { bid });
                        Ok(())
                    }
                    Some(slot) => Err(path_open(&path, slot.bid())),
                };
                let _ = reply.send(result);
            }
            ToRouter::CommitSaveAs { bid, old, new, reply } => {
                // The old path's slot moves with the buffer WHATEVER its state:
                // a close queued during the save left it `Closing`, and that
                // close now concerns the new path. Opens parked on the old path
                // re-run as fresh opens — it is no longer this buffer's file.
                let mut rerun = Vec::new();
                if let Some(old) = &old
                    && self.paths.get(old).is_some_and(|slot| *slot.bid() == bid)
                    && let Some(PathSlot::Closing { waiters, .. } | PathSlot::Loading { waiters, .. }) =
                        self.paths.remove(old)
                {
                    rerun = waiters;
                }
                let closing = self.entries.get(&bid).is_some_and(|e| e.closing);
                let slot = if closing {
                    PathSlot::Closing { bid: bid.clone(), waiters: vec![] }
                } else {
                    PathSlot::Bound { bid: bid.clone() }
                };
                self.paths.insert(new.clone(), slot);
                if let Some(entry) = self.entries.get_mut(&bid) {
                    let had_path = entry.path.replace(new.clone()).is_some();
                    if had_path {
                        self.watch.rebind(&bid, &new);
                    } else {
                        self.watch.add(&bid, &new, entry.signal.clone());
                    }
                }
                let _ = reply.send(());
                for w in rerun {
                    self.open(w);
                }
            }
            ToRouter::ReleaseSaveAs { path } => {
                if matches!(self.paths.get(&path), Some(PathSlot::SaveAs { .. })) {
                    self.paths.remove(&path);
                }
            }
            ToRouter::CloseDecided { bid, result, reply } => match result {
                Ok(()) => send(reply, Ok(json_of(&CloseReply { buffer: bid, closed: true, holders: vec![] }))),
                Err(refusal) => {
                    if let Some(entry) = self.entries.get_mut(&bid) {
                        entry.closing = false;
                    }
                    let waiters = self.rebind_after_refused_close(&bid);
                    send(reply, Err(refusal));
                    for w in waiters {
                        self.open(w);
                    }
                }
            },
            ToRouter::Closed { bid } => self.cleanup(&bid),
        }
    }

    fn rebind_after_refused_close(&mut self, bid: &str) -> Vec<OpenWaiter> {
        let Some(path) = self.entries.get(bid).and_then(|e| e.path.clone()) else { return vec![] };
        match self.paths.insert(path, PathSlot::Bound { bid: bid.to_string() }) {
            Some(PathSlot::Closing { waiters, .. }) => waiters,
            _ => vec![],
        }
    }

    /// Spawn an actor. A scratch open's waiter rides in the entry (it has no
    /// path slot to park in).
    fn spawn(&mut self, bid: BufferId, path: Option<PathBuf>, init: Init, leased: u64, first: Option<OpenWaiter>) {
        let (tx, rx) = mpsc::channel(ACTOR_INBOX);
        let signal = DiskSignal::new();
        self.entries.insert(
            bid.clone(),
            Entry {
                path,
                holders: vec![],
                props: None,
                signal: signal.clone(),
                tx,
                closing: false,
                pending: first.into_iter().collect(),
            },
        );
        let init = ActorInit {
            bid: bid.clone(),
            epoch: self.epoch.clone(),
            init,
            budget: self.budget.clone(),
            leased,
            publisher: self.publisher.clone(),
            to_router: self.internal_tx.clone(),
            signal,
            snapshot_seq: self.snapshot_seq.clone(),
            rx,
        };
        let handle = tokio::spawn(crate::actor::run(init));
        let internal = self.internal_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = handle.await
                && error.is_panic()
            {
                tracing::error!("cosmix-editd: buffer {bid} actor panicked; its unsaved text is lost");
            }
            let _ = internal.send(ToRouter::Closed { bid });
        });
    }

    fn open(&mut self, w: OpenWaiter) {
        let Some(path) = w.canonical.clone() else {
            if self.buffer_count >= MAX_BUFFERS {
                return send(w.reply, Err(too_many_buffers()));
            }
            let bid = self.buffer_id();
            self.buffer_count += 1;
            let language = w.language.clone();
            self.spawn(bid, None, Init::Scratch { language }, 0, Some(w));
            return;
        };
        match self.paths.get_mut(&path) {
            Some(PathSlot::Bound { bid }) => {
                let bid = bid.clone();
                self.reopen(&bid, w);
            }
            Some(PathSlot::Loading { waiters, .. }) | Some(PathSlot::Closing { waiters, .. }) => waiters.push(w),
            Some(PathSlot::SaveAs { bid }) => {
                let refusal = path_open(&path, bid);
                send(w.reply, Err(refusal));
            }
            None => {
                if self.buffer_count >= MAX_BUFFERS {
                    return send(w.reply, Err(too_many_buffers()));
                }
                let size = w.size.unwrap_or(0);
                if !self.budget.try_lease(size) {
                    return send(w.reply, Err(crate::refusal::budget(size)));
                }
                let bid = self.buffer_id();
                self.buffer_count += 1;
                let init = Init::Load {
                    path: path.clone(),
                    opened_as: w.opened_as.clone().unwrap_or_else(|| path.display().to_string()),
                    create: w.create,
                    language: w.language.clone(),
                };
                self.paths.insert(path.clone(), PathSlot::Loading { bid: bid.clone(), waiters: vec![w] });
                self.spawn(bid, Some(path), init, size, None);
            }
        }
    }

    fn add_holder(&mut self, bid: &str, holder: &str) {
        let Some(entry) = self.entries.get_mut(bid) else { return };
        if entry.holders.iter().any(|h| h == holder) {
            return;
        }
        let before = entry.holders.clone();
        entry.holders.push(holder.to_string());
        let after = entry.holders.clone();
        if let Some(props) = entry.props.clone() {
            self.publish_props(bid, Some((&props, before.as_slice())), Some((&props, after.as_slice())));
        }
    }

    fn open_reply(&self, bid: &str, reopened: bool, created: bool) -> Result<String, Refusal> {
        let p = self.entries.get(bid).and_then(|e| e.props.as_ref()).ok_or_else(|| unknown_buffer(bid))?;
        Ok(json_of(&OpenReply {
            buffer: bid.to_string(),
            epoch: self.epoch.clone(),
            path: p.path.clone(),
            opened_as: p.opened_as.clone(),
            name: p.name.clone(),
            language: p.language.clone(),
            rev: p.rev,
            lines: p.lines,
            bytes: p.bytes,
            eol: p.eol,
            bom: p.bom,
            disk: p.disk,
            reopened,
            created,
            recovery_id: p.recovery_id.clone(),
            recovered: p.recovered,
            // Filled from the recovery meta by the E1c restore path.
            recovered_from: None,
        }))
    }

    fn reopen(&mut self, bid: &str, w: OpenWaiter) {
        if let Some(r) = self.holder_refusal(bid, &w.caller) {
            return send(w.reply, Err(r));
        }
        self.add_holder(bid, &w.caller);
        send(w.reply, self.open_reply(bid, true, false));
    }

    fn loaded(&mut self, bid: BufferId, props: BufferProps, created: bool) {
        let Some(entry) = self.entries.get_mut(&bid) else { return };
        entry.props = Some(props.clone());
        let path = entry.path.clone();
        let signal = entry.signal.clone();
        let tx = entry.tx.clone();
        let mut waiters = std::mem::take(&mut entry.pending);
        if let Some(path) = &path
            && let Some(PathSlot::Loading { waiters: parked, .. }) =
                self.paths.insert(path.clone(), PathSlot::Bound { bid: bid.clone() })
        {
            waiters = parked;
        }
        // The `open` event is queued BEFORE the buffer becomes reachable, so
        // none of the buffer's own events can precede it.
        self.publisher.event(
            Some(&bid),
            Event::Open(OpenEvent {
                epoch: self.epoch.clone(),
                buffer: bid.clone(),
                path: props.path.clone(),
                rev: props.rev,
                event_seq: 0,
            }),
        );
        self.actors.write().expect("actor table").insert(bid.clone(), tx);
        if let Some(path) = &path {
            self.watch.add(&bid, path, signal);
        }
        self.publish_count(self.buffer_count.saturating_sub(1), self.buffer_count);
        self.publish_props(&bid, None, Some((&props, &[])));
        for (i, w) in waiters.into_iter().enumerate() {
            if let Some(r) = self.holder_refusal(&bid, &w.caller) {
                send(w.reply, Err(r));
                continue;
            }
            self.add_holder(&bid, &w.caller);
            send(w.reply, self.open_reply(&bid, i > 0, created && i == 0));
        }
    }

    /// RESOURCE_LIMIT `limit` when `holder` would be one holder too many
    /// (holders are listed in `edit.list` and props, so they are bounded).
    fn holder_refusal(&self, bid: &str, holder: &str) -> Option<Refusal> {
        let entry = self.entries.get(bid)?;
        (entry.holders.len() >= MAX_HOLDERS && !entry.holders.iter().any(|h| h == holder)).then(|| {
            refusal(ErrorCode::ResourceLimit, Some(reason::LIMIT), format!("{MAX_HOLDERS} callers already hold {bid}"))
                .buffer(bid)
                .with("limit", MAX_HOLDERS)
        })
    }

    fn failed(&mut self, bid: BufferId, refusal: Refusal) {
        let Some(entry) = self.entries.remove(&bid) else { return };
        self.buffer_count = self.buffer_count.saturating_sub(1);
        let mut waiters = entry.pending;
        if let Some(path) = &entry.path
            && let Some(PathSlot::Loading { waiters: parked, .. }) = self.paths.remove(path)
        {
            waiters = parked;
        }
        for w in waiters {
            send(w.reply, Err(refusal.clone()));
        }
    }

    fn close(&mut self, bid: BufferId, force: bool, caller: String, reply: oneshot::Sender<Reply>) {
        let Some(entry) = self.entries.get_mut(&bid) else {
            return send(reply, Err(unknown_buffer(&bid)));
        };
        if entry.closing {
            return send(
                reply,
                Err(refusal(ErrorCode::ResourceLimit, Some(reason::BUSY), format!("buffer {bid} is already closing"))
                    .buffer(&bid)),
            );
        }
        let remaining: Vec<String> = entry.holders.iter().filter(|h| **h != caller).cloned().collect();
        if !remaining.is_empty() && !force {
            let before = std::mem::replace(&mut entry.holders, remaining.clone());
            if let Some(props) = entry.props.clone() {
                self.publish_props(&bid, Some((&props, before.as_slice())), Some((&props, remaining.as_slice())));
            }
            return send(reply, Ok(json_of(&CloseReply { buffer: bid, closed: false, holders: remaining })));
        }
        match entry.tx.try_send(ActorMsg::Close { force, reply }) {
            Ok(()) => {
                entry.closing = true;
                if let Some(path) = entry.path.clone() {
                    self.paths.insert(path, PathSlot::Closing { bid, waiters: vec![] });
                }
            }
            Err(mpsc::error::TrySendError::Full(ActorMsg::Close { reply, .. })) => {
                send(reply, Err(busy(&bid, ACTOR_INBOX)));
            }
            Err(mpsc::error::TrySendError::Closed(ActorMsg::Close { reply, .. })) => {
                send(reply, Err(unknown_buffer(&bid)));
            }
            Err(_) => {}
        }
    }

    /// The actor task ended: drop every trace of the buffer and serve parked opens.
    fn cleanup(&mut self, bid: &str) {
        let Some(entry) = self.entries.remove(bid) else { return };
        if !entry.closing {
            tracing::error!("cosmix-editd: buffer {bid} ended without a close");
        }
        self.actors.write().expect("actor table").remove(bid);
        let before = self.buffer_count;
        self.buffer_count = self.buffer_count.saturating_sub(1);
        self.watch.remove(bid);
        let mut rerun = Vec::new();
        for w in entry.pending {
            send(w.reply, Err(crate::refusal::internal(format!("buffer {bid} failed while opening"))));
        }
        self.paths.retain(|_, slot| {
            if slot.bid() != bid {
                return true;
            }
            match std::mem::replace(slot, PathSlot::SaveAs { bid: String::new() }) {
                PathSlot::Closing { waiters, .. } => rerun.extend(waiters),
                PathSlot::Loading { waiters, .. } => {
                    for w in waiters {
                        send(w.reply, Err(crate::refusal::internal(format!("buffer {bid} failed while opening"))));
                    }
                }
                _ => {}
            }
            false
        });
        if entry.props.is_some() {
            self.publisher.event(
                Some(bid),
                Event::Close(CloseEvent { epoch: self.epoch.clone(), buffer: bid.to_string(), event_seq: 0 }),
            );
            let props = entry.props.clone();
            self.publish_props(bid, props.as_ref().map(|p| (p, entry.holders.as_slice())), None);
            self.publish_count(before, self.buffer_count);
        }
        for w in rerun {
            self.open(w);
        }
    }

    fn publish_count(&self, old: usize, new: usize) {
        if old != new
            && let Ok(path) = cosmix_props_core::PropPath::new("buffer_count")
        {
            self.publisher.props_changed(None, &path, &(old as u64).into(), &(new as u64).into());
        }
    }

    /// `props.changed` for one buffer's changed NON-transient leaves only.
    fn publish_props(&self, bid: &str, old: Option<(&BufferProps, &[String])>, new: Option<(&BufferProps, &[String])>) {
        let old = old.map(|(p, h)| buffer_leaves(bid, p, h));
        let new = new.map(|(p, h)| buffer_leaves(bid, p, h));
        let len = old.as_ref().or(new.as_ref()).map(Vec::len).unwrap_or(0);
        let null = cosmix_props_core::PropValue::Null;
        for i in 0..len {
            let o = old.as_ref().map(|l| &l[i]);
            let n = new.as_ref().map(|l| &l[i]);
            let (path, transient) = match (o, n) {
                (Some((p, _, t)), _) | (None, Some((p, _, t))) => (p, *t),
                (None, None) => continue,
            };
            if transient {
                continue;
            }
            let ov = o.map(|(_, v, _)| v).unwrap_or(&null);
            let nv = n.map(|(_, v, _)| v).unwrap_or(&null);
            if ov != nv {
                self.publisher.props_changed(Some(bid), path, ov, nv);
            }
        }
    }

    fn props_tree(&self) -> EditProps {
        let buffers = self
            .entries
            .iter()
            .filter_map(|(bid, e)| e.props.clone().map(|p| (bid.clone(), (p, e.holders.clone()))))
            .collect();
        EditProps::build(&self.epoch, self.publisher.event_seq(), self.publisher.loss(), &buffers)
    }

    fn info(&self) -> String {
        let build = cosmix_buildinfo::build_info!();
        let loaded = self.entries.values().filter_map(|e| e.props.as_ref());
        let (buffers, dirty) = loaded.fold((0, 0), |(n, d), p| (n + 1, d + usize::from(p.dirty)));
        let limits = json!({
            "max_buffer_bytes": cosmix_edit_core::limits::MAX_BUFFER_BYTES,
            "max_lines": cosmix_edit_core::limits::MAX_LINES,
            "max_request_text_bytes": cosmix_edit_core::limits::MAX_REQUEST_TEXT_BYTES,
            "max_ops_per_txn": cosmix_edit_core::limits::MAX_OPS_PER_TXN,
            "max_buffers": MAX_BUFFERS,
            "max_total_bytes": MAX_TOTAL_BYTES,
            "max_reply_bytes": MAX_REPLY_BYTES,
            "max_event_bytes": MAX_EVENT_BYTES,
            "budget_used": self.budget.used(),
        });
        json_of(&InfoReply {
            name: SERVICE.into(),
            schema: SCHEMA.into(),
            epoch: self.epoch.clone(),
            props_level: "L2".into(),
            binary: build.pkg.into(),
            version: build.version.into(),
            git_sha: build.git_sha.into(),
            git_dirty: build.git_dirty,
            build_time: build.build_time.into(),
            buffers,
            dirty,
            volatile: true,
            mesh_open: self.mesh_open,
            event_seq: self.publisher.event_seq(),
            publisher_loss: self.publisher.loss(),
            limits: limits.as_object().cloned().unwrap_or_default(),
            // Stage S: recovery files land in E1c; until then the daemon is
            // volatile and reports recovery disabled.
            recovery: RecoveryInfo { ok: true, sync_ms: crate::recovery::SYNC_MS, ..RecoveryInfo::default() },
        })
    }

    fn list(&self) -> String {
        let buffers = self
            .entries
            .iter()
            .filter_map(|(bid, e)| {
                let p = e.props.as_ref()?;
                Some(BufferSummary {
                    buffer: bid.clone(),
                    path: p.path.clone(),
                    opened_as: p.opened_as.clone(),
                    name: p.name.clone(),
                    language: p.language.clone(),
                    rev: p.rev,
                    saved_rev: p.saved_rev,
                    dirty: p.dirty,
                    disk: p.disk,
                    lines: p.lines,
                    bytes: p.bytes,
                    holders: e.holders.clone(),
                    recovery_id: p.recovery_id.clone(),
                    recovered: p.recovered,
                })
            })
            .collect();
        json_of(&ListReply { epoch: self.epoch.clone(), buffers })
    }
}

fn too_many_buffers() -> Refusal {
    refusal(ErrorCode::ResourceLimit, Some(reason::TOO_MANY_BUFFERS), format!("{MAX_BUFFERS} buffers are open"))
}

fn path_open(path: &std::path::Path, other: &str) -> Refusal {
    let shown = path.display().to_string();
    refusal(ErrorCode::Conflict, Some(reason::PATH_OPEN), format!("{shown} is bound to buffer {other}"))
        .with("other_buffer", other.to_string())
        .with("path", shown)
}

// ── the front door ──────────────────────────────────────────────────────────

/// Daemon configuration (fixed at start).
#[derive(Debug, Clone)]
pub struct Config {
    /// 8 lowercase hex, fresh per start.
    pub epoch: String,
    pub mesh_open: bool,
    pub budget_cap: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            epoch: format!("{:08x}", rand::random::<u32>()),
            mesh_open: caller::mesh_open_from_env(),
            budget_cap: MAX_TOTAL_BYTES,
        }
    }
}

pub type ReplyFuture = Pin<Box<dyn Future<Output = Reply> + Send>>;

/// The running daemon minus its Bus connection (so tests drive it in-process).
pub struct Editd {
    epoch: String,
    mesh_open: bool,
    router_tx: mpsc::Sender<RouterCmd>,
    actors: ActorTable,
    publisher: Arc<Publisher>,
    budget: Arc<Budget>,
}

fn ready(reply: Reply) -> ReplyFuture {
    Box::pin(std::future::ready(reply))
}

fn refused(r: Refusal) -> ReplyFuture {
    ready(render(&r))
}

/// `args` header, then the parsed args, then the raw body (powerd's order).
pub fn resolve_args(cmd: &IncomingCommand) -> Option<Value> {
    if let Some(args) = cmd.header("args")
        && let Ok(value) = serde_json::from_str(args)
    {
        return Some(value);
    }
    if !cmd.args.is_null() {
        return Some(cmd.args.clone());
    }
    if !cmd.body.is_empty()
        && let Ok(value) = serde_json::from_str(&cmd.body)
    {
        return Some(value);
    }
    None
}

fn parse<T: DeserializeOwned>(verb: &str, args: &Value) -> Result<T, Refusal> {
    serde_json::from_value(args.clone()).map_err(|e| bad_args(format!("{verb}: {e}")))
}

fn check_op_id(op_id: Option<&str>) -> Result<(), Refusal> {
    match op_id {
        Some(id)
            if id.is_empty()
                || id.len() > 64
                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')) =>
        {
            Err(refusal(
                ErrorCode::InvalidArgument,
                Some(reason::BAD_OP_ID),
                format!("op_id {id:?} must match ^[A-Za-z0-9._:-]{{1,64}}$"),
            ))
        }
        _ => Ok(()),
    }
}

fn check_cas(cas: &CasArgs) -> Result<(), Refusal> {
    if cas.expect_rev.is_some() && cas.base_rev.is_some() {
        return Err(refusal(ErrorCode::InvalidArgument, Some(reason::BOTH_CAS), "pass expect_rev or base_rev, not both"));
    }
    Ok(())
}

/// A `language` override is an id, not free text (it is repeated in every
/// list entry and props leaf of the buffer).
fn check_language(language: Option<&str>) -> Result<(), Refusal> {
    match language {
        Some(l)
            if l.is_empty()
                || l.len() > LANGUAGE_MAX
                || !l.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'#' | b'-')) =>
        {
            Err(bad_args(format!(
                "language must match ^[A-Za-z0-9._+#-]{{1,{LANGUAGE_MAX}}}$ (got {} bytes)",
                l.len()
            )))
        }
        _ => Ok(()),
    }
}

/// The last line of defence for every reply: an encoded body over
/// `MAX_REPLY_BYTES` is never sent (inputs are bounded so none should be).
///
/// A too-large SUCCESS may be a mutation that already applied, so it must not
/// look like a refusal a client would retry under a new op_id: it becomes a
/// compact success (`rc 0`) keeping `buffer`, `epoch`, `rev`, `op_id` and
/// flagged `reply_truncated: true` (refetch with `edit.get` / `edit.history`).
fn bounded(reply: Reply) -> Reply {
    if reply.1.len() <= MAX_REPLY_BYTES {
        return reply;
    }
    tracing::error!("cosmix-editd: a {}-byte reply exceeded MAX_REPLY_BYTES", reply.1.len());
    if reply.0 == 0 {
        let full: Value = serde_json::from_str(&reply.1).unwrap_or(Value::Null);
        let mut compact = serde_json::Map::new();
        for key in ["buffer", "epoch", "rev", "op_id", "duplicate"] {
            // Only short scalars survive (ids, numbers, flags).
            if let Some(v) = full.get(key).filter(|v| !v.is_object() && !v.is_array() && crate::events::encoded_len(v) <= 256) {
                compact.insert(key.to_string(), v.clone());
            }
        }
        compact.insert("reply_truncated".into(), Value::Bool(true));
        compact.insert("reply_bytes".into(), json!(reply.1.len()));
        return (0, Value::Object(compact).to_string());
    }
    render(
        &refusal(
            ErrorCode::ResourceLimit,
            Some(reason::LIMIT),
            format!("the reply would be {} bytes; the limit is {MAX_REPLY_BYTES}", reply.1.len()),
        )
        .with("limit", MAX_REPLY_BYTES),
    )
}

fn check_name(name: &str) -> Result<(), Refusal> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(refusal(
            ErrorCode::InvalidArgument,
            Some(reason::BAD_NAME),
            format!("anchor name {name:?} must match ^[A-Za-z0-9._-]{{1,64}}$"),
        ))
    }
}

/// Parsed buffer verb plus the claim that names the caller's origin.
fn parse_buffer_verb(verb: &str, args: &Value) -> Result<(BufferId, BufVerb, Option<String>), Refusal> {
    fn meta(m: &MutMeta) -> Result<Option<String>, Refusal> {
        check_op_id(m.op_id.as_deref())?;
        Ok(m.origin.clone())
    }
    Ok(match verb {
        "edit.save" => {
            let r: SaveReq = parse(verb, args)?;
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::Save(r), claim)
        }
        "edit.reload" => {
            let r: ReloadReq = parse(verb, args)?;
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::Reload(r), claim)
        }
        "edit.get" => {
            let r: GetReq = parse(verb, args)?;
            (r.buffer.clone(), BufVerb::Get(r), None)
        }
        "edit.insert" => {
            let r: InsertReq = parse(verb, args)?;
            check_cas(&r.args.cas)?;
            let claim = meta(&r.args.meta)?;
            (r.buffer.clone(), BufVerb::Insert(r), claim)
        }
        "edit.delete" => {
            let r: DeleteReq = parse(verb, args)?;
            check_cas(&r.args.cas)?;
            let claim = meta(&r.args.meta)?;
            (r.buffer.clone(), BufVerb::Delete(r), claim)
        }
        "edit.replace" => {
            let r: ReplaceReq = parse(verb, args)?;
            check_cas(&r.args.cas)?;
            let claim = meta(&r.args.meta)?;
            (r.buffer.clone(), BufVerb::Replace(r), claim)
        }
        "edit.apply" => {
            let r: ApplyReq = parse(verb, args)?;
            check_cas(&r.args.cas)?;
            let claim = meta(&r.args.meta)?;
            (r.buffer.clone(), BufVerb::Apply(r), claim)
        }
        "edit.find" => {
            let r: FindReq = parse(verb, args)?;
            (r.buffer.clone(), BufVerb::Find(r), None)
        }
        "edit.select" => {
            let r: SelectReq = parse(verb, args)?;
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::Select(r), claim)
        }
        "edit.cursor" => {
            let r: CursorReq = parse(verb, args)?;
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::Cursor(r), claim)
        }
        "edit.anchor.set" => {
            let r: AnchorSetReq = parse(verb, args)?;
            check_name(&r.name)?;
            if r.at.is_some() == r.range.is_some() {
                return Err(bad_args("edit.anchor.set takes exactly one of at / range"));
            }
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::AnchorSet(r), claim)
        }
        "edit.anchor.get" => {
            let r: AnchorGetReq = parse(verb, args)?;
            if let Some(name) = &r.name {
                check_name(name)?;
            }
            (r.buffer.clone(), BufVerb::AnchorGet(r), None)
        }
        "edit.anchor.clear" => {
            let r: AnchorClearReq = parse(verb, args)?;
            check_name(&r.name)?;
            let claim = meta(&r.meta)?;
            (r.buffer.clone(), BufVerb::AnchorClear(r), claim)
        }
        "edit.undo" | "edit.redo" => {
            let r: UndoReq = parse(verb, args)?;
            check_op_id(r.op_id.as_deref())?;
            if let Some(lane) = r.origin.as_deref()
                && lane != "*"
            {
                lane.parse::<cosmix_edit_core::origin::Origin>()
                    .map_err(|e| crate::refusal::from_core(e, Some(&r.buffer)))?;
            }
            let claim = r.as_.clone();
            let bid = r.buffer.clone();
            (bid, if verb == "edit.undo" { BufVerb::Undo(r) } else { BufVerb::Redo(r) }, claim)
        }
        "edit.history" => {
            let r: HistoryReq = parse(verb, args)?;
            (r.buffer.clone(), BufVerb::History(r), None)
        }
        _ => return Err(refusal(ErrorCode::UnknownVerb, None, format!("unknown edit verb: {verb}"))),
    })
}

impl Editd {
    /// Start the router, publisher and watcher on the current tokio runtime.
    pub fn start(config: Config, sink: Arc<dyn EventSink>) -> Arc<Editd> {
        Self::start_with(config, None, sink)
    }

    /// As [`start`](Self::start), with a caller-built publisher (tests shrink
    /// its queue budget).
    pub fn start_with(config: Config, publisher: Option<Arc<Publisher>>, sink: Arc<dyn EventSink>) -> Arc<Editd> {
        let publisher = publisher.unwrap_or_else(|| Publisher::new(&config.epoch));
        tokio::spawn(publisher.clone().run(sink));
        let budget = Arc::new(Budget::new(config.budget_cap));
        let actors: ActorTable = Arc::new(RwLock::new(HashMap::new()));
        let (router_tx, router_rx) = mpsc::channel(ROUTER_INBOX);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let router = Router {
            epoch: config.epoch.clone(),
            paths: PathTable::new(),
            budget: budget.clone(),
            buffer_count: 0,
            next_buffer: 0,
            mesh_open: config.mesh_open,
            entries: BTreeMap::new(),
            actors: actors.clone(),
            publisher: publisher.clone(),
            watch: Watch::start(),
            internal_tx,
            snapshot_seq: Arc::new(AtomicU64::new(0)),
        };
        tokio::spawn(router.run(router_rx, internal_rx));
        Arc::new(Editd { epoch: config.epoch, mesh_open: config.mesh_open, router_tx, actors, publisher, budget })
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn publisher(&self) -> &Arc<Publisher> {
        &self.publisher
    }

    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }

    /// Route and await one command.
    pub async fn handle(&self, cmd: &IncomingCommand) -> Reply {
        self.submit(cmd).await
    }

    /// Checks 1-6 of the precedence, synchronously and in call order, then the
    /// hand-off; the returned future only awaits the owner's answer (and
    /// bounds it: see [`bounded`]).
    pub fn submit(&self, cmd: &IncomingCommand) -> ReplyFuture {
        let reply = self.route(cmd);
        Box::pin(async move { bounded(reply.await) })
    }

    fn route(&self, cmd: &IncomingCommand) -> ReplyFuture {
        let verb = cmd.command.as_str();
        let args = resolve_args(cmd).unwrap_or_else(|| json!({}));
        let named_buffer = args.get("buffer").and_then(Value::as_str).map(str::to_string);
        let Some(read_only) = VERBS.iter().find(|(v, _)| *v == verb).map(|(_, ro)| *ro) else {
            let mut r = refusal(ErrorCode::UnknownVerb, None, format!("unknown edit verb: {verb}"));
            r.buffer = named_buffer;
            return refused(r);
        };
        let mutating = !read_only;
        let with_buffer = |mut r: Refusal| {
            if r.buffer.is_none() {
                r.buffer = named_buffer.clone();
            }
            r
        };
        match verb {
            "edit.ping" => ready((
                0,
                json_of(&PingReply {
                    pong: true,
                    service: SERVICE.into(),
                    schema: SCHEMA.into(),
                    epoch: self.epoch.clone(),
                }),
            )),
            "edit.props.watch" => ready((
                0,
                json_of(&PropsWatchReply {
                    topic: TOPIC_PROPS_CHANGED.into(),
                    domain_topics: vec![TOPIC_CHANGED.into()],
                    event_seq: self.publisher.event_seq(),
                    event_sequence: "daemon_session_monotonic".into(),
                    loss_signal: "resync_event".into(),
                    bootstrap: "subscribe, then edit.get snapshot:true, then apply buffered edit events with base_rev >= snapshot rev".into(),
                }),
            )),
            "edit.info" => self.to_router(|reply| RouterCmd::Info { reply }),
            // ced E1 plan §5.1; implemented by Stage E1c.
            "edit.recovery.flush" => refused(refusal(
                ErrorCode::Internal,
                None,
                "edit.recovery.flush is not implemented yet (ced E1 Stage E1c)",
            )),
            "edit.list" => self.to_router(|reply| RouterCmd::List { reply }),
            "edit.props.get" | "edit.props.list" | "edit.props.describe" => {
                let suffix = verb.trim_start_matches("edit.props.").to_string();
                let args = Some(args);
                self.to_router(move |reply| RouterCmd::Props { suffix, args, reply })
            }
            "edit.open" => {
                let req: OpenReq = match parse(verb, &args) {
                    Ok(r) => r,
                    Err(r) => return refused(r),
                };
                if let Err(r) = check_op_id(req.meta.op_id.as_deref()) {
                    return refused(r);
                }
                if let Err(r) = check_language(req.language.as_deref()) {
                    return refused(r);
                }
                let caller = match caller::resolve(cmd, req.meta.origin.as_deref(), mutating, self.mesh_open) {
                    Ok(c) => c,
                    Err(r) => return refused(r),
                };
                self.open(req, caller)
            }
            "edit.close" => {
                let req: CloseReq = match parse(verb, &args) {
                    Ok(r) => r,
                    Err(r) => return refused(with_buffer(r)),
                };
                if let Err(r) = check_op_id(req.meta.op_id.as_deref()) {
                    return refused(with_buffer(r));
                }
                let caller = match caller::resolve(cmd, req.meta.origin.as_deref(), mutating, self.mesh_open) {
                    Ok(c) => c,
                    Err(r) => return refused(with_buffer(r)),
                };
                if let Err(r) = self.lookup(&req.buffer) {
                    return refused(r);
                }
                let (bid, force, key) = (req.buffer, req.force, caller::holder_key(&caller.key));
                self.to_router(move |reply| RouterCmd::Close { bid, force, caller: key, reply })
            }
            _ => {
                let (bid, bv, claim) = match parse_buffer_verb(verb, &args) {
                    Ok(p) => p,
                    Err(r) => return refused(with_buffer(r)),
                };
                let caller = match caller::resolve(cmd, claim.as_deref(), mutating, self.mesh_open) {
                    Ok(c) => c,
                    Err(r) => return refused(with_buffer(r)),
                };
                self.to_actor(&bid, bv, caller)
            }
        }
    }

    fn to_router(&self, make: impl FnOnce(oneshot::Sender<Reply>) -> RouterCmd) -> ReplyFuture {
        let (tx, rx) = oneshot::channel();
        match self.router_tx.try_send(make(tx)) {
            Ok(()) => Box::pin(async move {
                rx.await.unwrap_or_else(|_| render(&crate::refusal::internal("the router went away")))
            }),
            Err(mpsc::error::TrySendError::Full(_)) => refused(router_busy()),
            Err(mpsc::error::TrySendError::Closed(_)) => refused(crate::refusal::internal("the router went away")),
        }
    }

    /// Check 5: the id's epoch, then the actor table.
    fn lookup(&self, bid: &str) -> Result<mpsc::Sender<ActorMsg>, Refusal> {
        if let Some((head, epoch)) = bid.rsplit_once('_')
            && epoch != self.epoch
            && head.starts_with('b')
            && epoch.len() == 8
            && epoch.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(refusal(
                ErrorCode::NotFound,
                Some(reason::EPOCH_MISMATCH),
                format!("buffer {bid} belongs to an earlier editd (epoch {epoch}, now {})", self.epoch),
            )
            .buffer(bid)
            .with("epoch", self.epoch.clone()));
        }
        self.actors.read().expect("actor table").get(bid).cloned().ok_or_else(|| unknown_buffer(bid))
    }

    fn to_actor(&self, bid: &str, verb: BufVerb, caller: Caller) -> ReplyFuture {
        let tx = match self.lookup(bid) {
            Ok(tx) => tx,
            Err(r) => return refused(r),
        };
        let (reply, rx) = oneshot::channel();
        match tx.try_send(ActorMsg::Cmd { verb: Box::new(verb), caller, reply }) {
            Ok(()) => {
                let bid = bid.to_string();
                Box::pin(async move { rx.await.unwrap_or_else(|_| render(&unknown_buffer(&bid))) })
            }
            Err(mpsc::error::TrySendError::Full(_)) => refused(busy(bid, ACTOR_INBOX)),
            Err(mpsc::error::TrySendError::Closed(_)) => refused(unknown_buffer(bid)),
        }
    }

    /// `edit.open`: resolve + stat off the runtime, then the router.
    fn open(&self, req: OpenReq, caller: Caller) -> ReplyFuture {
        let router_tx = self.router_tx.clone();
        Box::pin(async move {
            let (canonical, size) = match req.path.clone() {
                None => (None, None),
                Some(path) => {
                    let resolved = tokio::task::spawn_blocking(move || {
                        crate::files::resolve_path(&path)
                            .map(|p| {
                                let size = std::fs::metadata(&p).ok().filter(|m| m.is_file()).map(|m| m.len());
                                (p, size)
                            })
                    })
                    .await;
                    match resolved {
                        Ok(Ok((p, size))) => (Some(p), size),
                        Ok(Err(r)) => return render(&r),
                        Err(e) => return render(&crate::refusal::internal(format!("resolving the path: {e}"))),
                    }
                }
            };
            let (tx, rx) = oneshot::channel();
            let waiter = OpenWaiter {
                caller: caller::holder_key(&caller.key),
                canonical,
                opened_as: req.path.clone(),
                size,
                create: req.create,
                language: req.language.clone(),
                reply: tx,
            };
            match router_tx.try_send(RouterCmd::Open(waiter)) {
                Ok(()) => rx.await.unwrap_or_else(|_| render(&crate::refusal::internal("the router went away"))),
                Err(mpsc::error::TrySendError::Full(_)) => render(&router_busy()),
                Err(mpsc::error::TrySendError::Closed(_)) => render(&crate::refusal::internal("the router went away")),
            }
        })
    }

    /// Every buffer with unsaved text (SIGTERM log).
    pub async fn dirty_buffers(&self) -> Vec<DirtyBuffer> {
        let (tx, rx) = oneshot::channel();
        if self.router_tx.send(RouterCmd::Dirty { reply: tx }).await.is_err() {
            return vec![];
        }
        rx.await.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_admits_only_what_fits() {
        let b = Budget::new(100);
        assert!(b.try_lease(60));
        assert!(!b.try_lease(41));
        assert!(b.try_lease(40));
        b.release(50);
        assert_eq!(b.used(), 50);
        assert!(!b.try_lease(u64::MAX));
    }

    #[test]
    fn budget_race_admits_only_what_fits() {
        let b = Arc::new(Budget::new(1_000));
        let wins: usize = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|_| s.spawn(|| (0..100).filter(|_| b.try_lease(3)).count())).collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(wins, 333);
        assert_eq!(b.used(), 999);
    }

    fn test_router() -> (Router, mpsc::UnboundedReceiver<ToRouter>) {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let router = Router {
            epoch: "00000000".into(),
            paths: PathTable::new(),
            budget: Arc::new(Budget::new(MAX_TOTAL_BYTES)),
            buffer_count: 1,
            next_buffer: 1,
            mesh_open: true,
            entries: BTreeMap::new(),
            actors: Arc::new(RwLock::new(HashMap::new())),
            publisher: Publisher::new("00000000"),
            watch: Watch::start(),
            internal_tx,
            snapshot_seq: Arc::new(AtomicU64::new(0)),
        };
        (router, internal_rx)
    }

    #[tokio::test]
    async fn save_as_carries_a_queued_close_to_the_new_path() {
        let (mut router, mut internal) = test_router();
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        let bid = "b1_00000000".to_string();
        let (tx, mut actor_rx) = mpsc::channel(ACTOR_INBOX);
        router.entries.insert(
            bid.clone(),
            Entry {
                path: Some(old.clone()),
                holders: vec!["t".into()],
                props: None,
                signal: DiskSignal::new(),
                tx,
                closing: false,
                pending: vec![],
            },
        );
        router.paths.insert(old.clone(), PathSlot::Bound { bid: bid.clone() });

        // A close queued while the save-as A → B is in flight.
        let (close_tx, _close_rx) = oneshot::channel();
        router.close(bid.clone(), false, "t".into(), close_tx);
        assert!(matches!(router.paths.get(&old), Some(PathSlot::Closing { .. })));
        assert!(matches!(actor_rx.try_recv(), Ok(ActorMsg::Close { .. })));
        // An open of A parks on the close.
        let (open_tx, open_rx) = oneshot::channel();
        router.open(OpenWaiter {
            caller: "o".into(),
            canonical: Some(old.clone()),
            opened_as: None,
            size: Some(0),
            create: false,
            language: None,
            reply: open_tx,
        });

        let (commit_tx, _commit_rx) = oneshot::channel();
        router.internal(ToRouter::CommitSaveAs { bid: bid.clone(), old: Some(old.clone()), new: new.clone(), reply: commit_tx });
        assert!(matches!(router.paths.get(&new), Some(PathSlot::Closing { .. })), "the close moved with the buffer");
        assert!(
            matches!(router.paths.get(&old), Some(PathSlot::Loading { bid: b, .. }) if *b != bid),
            "the parked open of A re-ran as a fresh open"
        );

        // The close is refused (the buffer is dirty): B is bound again.
        let (reply, _r) = oneshot::channel();
        let dirty = refusal(ErrorCode::Conflict, Some(reason::DIRTY), "dirty").buffer(&bid);
        router.internal(ToRouter::CloseDecided { bid: bid.clone(), result: Err(dirty), reply });
        assert!(matches!(router.paths.get(&new), Some(PathSlot::Bound { bid: b }) if *b == bid));

        // The re-run open of A is answered (A is gone from disk: not found)
        // instead of waiting on a slot nobody will ever clear.
        let mut open_rx = open_rx;
        let pump = async {
            loop {
                tokio::select! {
                    reply = &mut open_rx => break reply.unwrap(),
                    Some(msg) = internal.recv() => router.internal(msg),
                }
            }
        };
        let (rc, body) = tokio::time::timeout(std::time::Duration::from_secs(10), pump).await.expect("the open of A hung");
        assert_eq!(rc, 10, "{body}");
        assert!(body.contains("file_not_found"), "{body}");
        assert!(!router.paths.contains_key(&old));
    }

    #[test]
    fn worst_case_list_and_props_fit_the_reply_budget() {
        use crate::limits::{HOLDER_KEY_MAX, PATH_MAX_ENCODED_BYTES};
        // Control characters encode as 6 bytes: the longest encoding per byte.
        let path = format!("/{}", "\u{1}".repeat((PATH_MAX_ENCODED_BYTES - 3) / 6));
        assert!(crate::events::encoded_len(&path) <= PATH_MAX_ENCODED_BYTES);
        let holders: Vec<String> = (0..MAX_HOLDERS).map(|i| format!("{i:0>width$}", width = HOLDER_KEY_MAX)).collect();
        let props = BufferProps {
            path: Some(path.clone()),
            opened_as: Some(path.clone()),
            name: Some(path.clone()),
            language: "x".repeat(LANGUAGE_MAX),
            eol: cosmix_edit_core::buffer::Eol::Mixed,
            bom: true,
            dirty: true,
            saved_rev: Some(u64::MAX),
            disk: DiskState::Unwatched,
            rev: u64::MAX,
            lines: usize::MAX,
            bytes: usize::MAX,
            origin_last: Some(format!("agent:{}", "l".repeat(64))),
            recovery_id: "f".repeat(16),
            recovered: true,
        };
        let buffers: Vec<BufferSummary> = (0..MAX_BUFFERS)
            .map(|i| BufferSummary {
                buffer: format!("b{}_00000000", u64::MAX - i as u64),
                path: props.path.clone(),
                opened_as: props.opened_as.clone(),
                name: props.name.clone(),
                language: props.language.clone(),
                rev: props.rev,
                saved_rev: props.saved_rev,
                dirty: props.dirty,
                disk: props.disk,
                lines: props.lines,
                bytes: props.bytes,
                holders: holders.clone(),
                recovery_id: props.recovery_id.clone(),
                recovered: props.recovered,
            })
            .collect();
        let list = json_of(&ListReply { epoch: "00000000".into(), buffers: buffers.clone() });
        assert!(list.len() < MAX_REPLY_BYTES, "worst-case edit.list is {} bytes", list.len());
        let tree: BTreeMap<BufferId, (BufferProps, Vec<String>)> =
            buffers.iter().map(|b| (b.buffer.clone(), (props.clone(), holders.clone()))).collect();
        let tree = EditProps::build("00000000", u64::MAX, u64::MAX, &tree);
        let got = cosmix_props_core::bus::dispatch_props(&tree, "get", None, true);
        assert!(got.body.len() < MAX_REPLY_BYTES, "worst-case props.get is {} bytes", got.body.len());
        // The largest single props leaf (holders) is far under the event budget.
        let leaves = buffer_leaves("b1_00000000", &props, &holders);
        let null = cosmix_props_core::PropValue::Null;
        for (path, value, _) in &leaves {
            let m = cosmix_props_core::publish::build_props_changed_message(path, &null, value, "edit");
            assert!(m.body.len() < MAX_EVENT_BYTES / 8, "{path:?}: {} bytes", m.body.len());
        }
    }

    #[test]
    fn language_and_reply_bounds() {
        assert!(check_language(Some("rust")).is_ok());
        assert!(check_language(Some("c++")).is_ok());
        assert!(check_language(None).is_ok());
        assert_eq!(check_language(Some(&"x".repeat(3 << 20))).unwrap_err().reason.as_deref(), Some("bad_args"));
        assert!(check_language(Some("has space")).is_err());
        assert!(check_language(Some("")).is_err());
        // An oversized success (a mutation that applied) stays a success with
        // its rev and op_id, never a refusal a client would retry.
        let big = json!({"buffer": "b1_00000000", "epoch": "00000000", "rev": 7, "op_id": "k-1", "text": "x".repeat(MAX_REPLY_BYTES)});
        let (rc, body) = bounded((0, big.to_string()));
        assert_eq!(rc, 0);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["rev"].as_u64(), v["op_id"].as_str()), (Some(7), Some("k-1")));
        assert_eq!(v["reply_truncated"], true);
        assert!(v.get("text").is_none());
        // An oversized refusal stays a refusal.
        let (rc, body) = bounded((10, "x".repeat(MAX_REPLY_BYTES + 1)));
        assert_eq!(rc, 10);
        assert!(body.contains("RESOURCE_LIMIT"), "{body}");
        assert_eq!(bounded((0, "{}".into())), (0, "{}".to_string()));
    }

    #[test]
    fn op_id_grammar() {
        assert!(check_op_id(Some("k-118:x.y_z")).is_ok());
        assert!(check_op_id(None).is_ok());
        assert_eq!(check_op_id(Some("has space")).unwrap_err().reason.as_deref(), Some("bad_op_id"));
        assert!(check_op_id(Some(&"a".repeat(65))).is_err());
    }

    #[test]
    fn both_cas_is_an_argument_shape_error() {
        let args = json!({"buffer": "b1_00000000", "at": 0, "text": "x", "expect_rev": 1, "base_rev": 1});
        let err = parse_buffer_verb("edit.insert", &args).err().unwrap();
        assert_eq!(err.reason.as_deref(), Some("both_cas"));
        let err = parse_buffer_verb("edit.insert", &json!({"buffer": "b1_00000000"})).err().unwrap();
        assert_eq!(err.reason.as_deref(), Some("bad_args"));
    }
}
