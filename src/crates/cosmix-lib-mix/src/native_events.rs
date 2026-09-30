//! Evaluator-generation-owned native sources. Only owned Rust data crosses threads.
//! Notify is a readiness hint; records stay under the mutex until delivery commits.
use crate::{
    error::{MixError, MixResult},
    evaluator::IncomingEvent,
    value::Value,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::{cell::Cell, rc::Rc};
use tokio::sync::Notify;

pub(crate) const MAX_WATCHES: usize = 128;
pub(crate) const MAX_DIRS: usize = 8192;
pub(crate) const MAX_PENDING: usize = 4096;
pub(crate) const MAX_CHILDREN: usize = 128;
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub path: String,
    pub kind: &'static str,
    pub old_path: Option<String>,
}

#[derive(Default)]
struct PendingWatch {
    changes: BTreeMap<String, Change>,
    overflow: bool,
}

/// A net/audio handle's coalesced records (desktop_events.rs). Keyed by the
/// source's own identity (link index, address, facility#index): last wins.
#[derive(Default)]
struct PendingSource {
    command: &'static str,
    changes: BTreeMap<String, serde_json::Value>,
    overflow: bool,
    closed: Option<serde_json::Value>,
}

impl PendingSource {
    fn ready(&self) -> bool {
        !self.changes.is_empty() || self.overflow || self.closed.is_some()
    }
}

/// One socket subscription's ordered FIFO (ws_on/tcp_on). Ordered and
/// lossless — the antipode of the coalescing `sources` map above, which is
/// deliberately unsuitable for frames.
#[cfg(feature = "ws")]
#[derive(Default)]
struct PendingSocket {
    command: String,
    /// Which Class C recv verb may consume this source's records.
    kind: &'static str,
    frames: VecDeque<crate::builtins::socket_sources::SocketRecord>,
    bytes: usize,
    closed: Option<serde_json::Value>,
    /// At most one Class C recv may park on a source at a time.
    parked: bool,
}

#[derive(Default)]
struct Pending {
    watches: BTreeMap<String, PendingWatch>,
    sources: BTreeMap<String, PendingSource>,
    children: VecDeque<serde_json::Value>,
    /// Ordered socket frames (ws_on/tcp_on): one FIFO per source, bounded
    /// by MAX_SOCKET_FRAMES / MAX_SOCKET_BYTES across all sources —
    /// overflow hard-closes the source, never a silent drop.
    #[cfg(feature = "ws")]
    sockets: BTreeMap<String, PendingSocket>,
    #[cfg(feature = "ws")]
    last_socket: Option<String>,
    #[cfg(feature = "ws")]
    socket_frames: usize,
    #[cfg(feature = "ws")]
    socket_bytes: usize,
    count: usize,
    closed: bool,
    last_watch: Option<String>,
    last_source: Option<String>,
    /// Round-robin over filesystem (0), child (1), net/audio (2) and
    /// socket (3) records.
    turn: usize,
}

/// Which native families a consumer can dispatch. A sleep yield point only
/// takes records whose command has a registered handler.
#[derive(Clone, Copy, Default)]
pub(crate) struct Families {
    pub filesystem: bool,
    pub children: bool,
    pub net: bool,
    pub audio: bool,
    /// Socket subscriptions (ws_on/tcp_on). Their event commands are
    /// caller-chosen, so the family gate is source presence, not a
    /// handler-name lookup.
    pub sockets: bool,
}

impl Families {
    pub const ALL: Self = Self {
        filesystem: true,
        children: true,
        net: true,
        audio: true,
        sockets: true,
    };

    /// Only the `tokio-sleep` sleep pump selects on native events
    /// (evaluator.rs `sleep()`); without the feature it has no caller.
    #[cfg(feature = "tokio-sleep")]
    pub fn any(self) -> bool {
        self.filesystem || self.children || self.net || self.audio || self.sockets
    }

    fn source(self, command: &str) -> bool {
        match command {
            "net.changed" => self.net,
            "audio.changed" => self.audio,
            _ => false,
        }
    }
}

/// One Class C recv outcome on a socket source.
#[cfg(feature = "ws")]
#[derive(Debug)]
pub(crate) enum SocketNext {
    Frame(crate::builtins::socket_sources::SocketRecord),
    Closed(serde_json::Value),
    Idle,
}

#[derive(Default)]
pub(crate) struct Queue {
    pending: Mutex<Pending>,
    ready: Notify,
    pub directories: AtomicUsize,
}

impl Queue {
    #[cfg(all(test, feature = "ws"))]
    pub(crate) fn socket_snapshot(&self) -> (usize, usize, usize) {
        let p = self.pending.lock().unwrap();
        (p.sockets.len(), p.socket_frames, p.socket_bytes)
    }
    #[cfg(all(test, feature = "ws"))]
    pub(crate) fn socket_idle_for_test(&self, handle: &str) -> bool {
        self.pending.lock().unwrap().sockets.get(handle)
            .is_some_and(|s| s.frames.is_empty() && s.closed.is_none())
    }
    #[cfg(target_os = "linux")]
    pub fn overflow_watches(&self) {
        let mut p = self.pending.lock().unwrap();
        for watch in p.watches.values_mut() {
            watch.overflow = true;
        }
        drop(p);
        self.ready.notify_waiters();
    }

    pub fn change(&self, handle: &str, change: Option<Change>, overflow: bool) {
        let mut p = self.pending.lock().unwrap();
        let count = p.count;
        let Some(w) = p.watches.get_mut(handle) else {
            return;
        };
        w.overflow |= overflow;
        if let Some(mut c) = change {
            if let Some(previous) = w.changes.get(&c.path) {
                // Do not erase a pending creation or paired rename when a
                // write/close-write follows it: the stronger kind subsumes it.
                if c.kind == "modified" && matches!(previous.kind, "moved" | "created") {
                    c = previous.clone();
                } else if previous.old_path.is_some() && previous.old_path != c.old_path {
                    // A single per-path record cannot retain two different
                    // rename origins. Require a rescan rather than lose one.
                    w.overflow = true;
                }
            }
            if w.changes.contains_key(&c.path) || count < MAX_PENDING {
                if w.changes.insert(c.path.clone(), c).is_none() {
                    p.count += 1;
                }
            } else {
                w.overflow = true;
            }
        }
        drop(p);
        self.ready.notify_waiters();
    }

    pub fn child(&self, body: serde_json::Value) {
        let mut p = self.pending.lock().unwrap();
        if !p.closed {
            // One terminal record per admitted child; admission counts undelivered exits.
            p.children.push_back(body);
        }
        drop(p);
        self.ready.notify_waiters();
    }

    /// Coalesce a net/audio batch. Records beyond the per-handle bound are
    /// dropped and the handle's next batch says overflow (re-read state).
    pub fn source(&self, handle: &str, changes: Vec<(String, serde_json::Value)>, overflow: bool) {
        let mut p = self.pending.lock().unwrap();
        let Some(s) = p.sources.get_mut(handle) else {
            return;
        };
        s.overflow |= overflow;
        for (key, change) in changes {
            if s.changes.contains_key(&key)
                || s.changes.len() < crate::desktop_events::MAX_SOURCE_PENDING
            {
                s.changes.insert(key, change);
            } else {
                s.overflow = true;
            }
        }
        drop(p);
        self.ready.notify_waiters();
    }

    /// The source died on its own (event stream exited, socket error). One
    /// terminal batch carries `closed`; the handle stays until unwatched.
    pub fn source_closed(&self, handle: &str, reason: serde_json::Value) {
        let mut p = self.pending.lock().unwrap();
        let Some(s) = p.sources.get_mut(handle) else {
            return;
        };
        s.overflow = true;
        s.closed = Some(reason);
        drop(p);
        self.ready.notify_waiters();
    }

    /// Admit one socket frame. `false` means the byte/frame bound was hit:
    /// the caller MUST hard-close and publish exactly one terminal event —
    /// frames are never silently dropped.
    #[cfg(feature = "ws")]
    pub fn socket_push(
        &self,
        handle: &str,
        record: crate::builtins::socket_sources::SocketRecord,
    ) -> bool {
        let mut p = self.pending.lock().unwrap();
        if !p.sockets.contains_key(handle) {
            // Unsubscribed mid-push: the record dies with the subscription.
            return true;
        }
        let len = record.data.len();
        if p.socket_frames + 1 > crate::builtins::socket_sources::MAX_SOCKET_FRAMES
            || p.socket_bytes + len > crate::builtins::socket_sources::MAX_SOCKET_BYTES
        {
            return false;
        }
        let s = p.sockets.get_mut(handle).expect("socket slot checked above");
        s.bytes += len;
        s.frames.push_back(record);
        p.socket_bytes += len;
        p.socket_frames += 1;
        drop(p);
        self.ready.notify_waiters();
        true
    }

    /// One terminal record per source; further terminals are ignored.
    #[cfg(feature = "ws")]
    pub fn socket_closed(&self, handle: &str, reason: serde_json::Value) {
        let mut p = self.pending.lock().unwrap();
        if let Some(s) = p.sockets.get_mut(handle)
            && s.closed.is_none()
        {
            s.closed = Some(reason);
        }
        drop(p);
        self.ready.notify_waiters();
    }

    /// Class C recv on one source: the next ordered record, or the
    /// terminal. `expect` is the source kind the verb may consume; `max`
    /// bounds a bytes-mode take (the remainder stays queued, in order).
    #[cfg(feature = "ws")]
    pub async fn next_socket(
        &self,
        handle: &str,
        expect: &'static str,
        max: usize,
    ) -> MixResult<SocketNext> {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            {
                let mut p = self.pending.lock().unwrap();
                if p.closed {
                    return Err(refusal("NATIVE_CLOSED", "native sources retired"));
                }
                let Some(s) = p.sockets.get_mut(handle) else {
                    return Err(refusal(
                        "SOCKET_WATCH_HANDLE",
                        "unknown or retired socket source",
                    ));
                };
                if s.kind != expect {
                    return Err(refusal(
                        "SOCKET_KIND",
                        format!(
                            "source {handle} is a {} subscription — use the matching recv verb",
                            s.kind
                        ),
                    ));
                }
                if !s.frames.is_empty() {
                    let head = if s.frames.front().unwrap().data.len() <= max {
                        let rec = s.frames.pop_front().unwrap();
                        s.bytes -= rec.data.len();
                        p.socket_bytes -= rec.data.len();
                        p.socket_frames -= 1;
                        rec
                    } else {
                        let rec = s.frames.front_mut().unwrap();
                        let rest = rec.data.split_off(max);
                        let head = crate::builtins::socket_sources::SocketRecord {
                            kind: rec.kind,
                            data: std::mem::replace(&mut rec.data, rest),
                        };
                        s.bytes -= head.data.len();
                        p.socket_bytes -= head.data.len();
                        head
                    };
                    return Ok(SocketNext::Frame(head));
                }
                if let Some(closed) = s.closed.take() {
                    p.sockets.remove(handle);
                    return Ok(SocketNext::Closed(closed));
                }
            }
            ready.await;
        }
    }

    /// The event-pump consumer: whole records, in order, under the source's
    /// command; the terminal record removes the slot (retired).
    #[cfg(feature = "ws")]
    fn take_socket(p: &mut Pending, handle: &str) -> Option<IncomingEvent> {
        let s = p.sockets.get_mut(handle)?;
        if !s.frames.is_empty() {
            let rec = s.frames.pop_front().unwrap();
            s.bytes -= rec.data.len();
            p.socket_bytes -= rec.data.len();
            p.socket_frames -= 1;
            let data = match rec.kind {
                "text" | "line" => {
                    serde_json::Value::String(String::from_utf8_lossy(&rec.data).into_owned())
                }
                _ => serde_json::json!({
                    "hex": crate::builtins::socket_sources::hex_encode(&rec.data),
                }),
            };
            let command = s.command.clone();
            return Some(event(
                &command,
                serde_json::json!({"watch": handle, "frame": {"kind": rec.kind, "data": data}}),
            ));
        }
        if let Some(closed) = s.closed.take() {
            let command = s.command.clone();
            p.sockets.remove(handle);
            return Some(event(
                &command,
                serde_json::json!({"watch": handle, "closed": closed}),
            ));
        }
        None
    }

    #[cfg(all(test, feature = "ws"))]
    pub(crate) fn register_socket_for_test(&self, handle: &str, command: &str, kind: &'static str) {
        self.pending.lock().unwrap().sockets.insert(
            handle.into(),
            PendingSocket {
                command: command.into(),
                kind,
                ..Default::default()
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn register_source_for_test(&self, handle: &str, command: &'static str) {
        self.pending.lock().unwrap().sources.insert(
            handle.into(),
            PendingSource {
                command,
                ..Default::default()
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn source_ready_for_test(&self, handle: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .sources
            .get(handle)
            .is_some_and(PendingSource::ready)
    }

    fn take_source(p: &mut Pending, handle: &str) -> Option<IncomingEvent> {
        let s = p.sources.get_mut(handle)?;
        if !s.ready() {
            return None;
        }
        let changes: Vec<_> = std::mem::take(&mut s.changes).into_values().collect();
        let overflow = std::mem::take(&mut s.overflow);
        let mut body = serde_json::json!({"watch": handle, "changes": changes, "overflow": overflow});
        if let Some(closed) = s.closed.take() {
            body["closed"] = closed;
        }
        Some(event(s.command, body))
    }

    fn take_watch(p: &mut Pending, handle: &str) -> Option<serde_json::Value> {
        let w = p.watches.get_mut(handle)?;
        if w.changes.is_empty() && !w.overflow {
            return None;
        }
        let changes = std::mem::take(&mut w.changes);
        p.count -= changes.len();
        let overflow = std::mem::take(&mut w.overflow);
        let changes: Vec<_> = changes
            .into_values()
            .map(|c| {
                let mut v = serde_json::json!({"path": c.path, "kind": c.kind});
                if let Some(old) = c.old_path {
                    v["old_path"] = old.into();
                }
                v
            })
            .collect();
        Some(serde_json::json!({"watch": handle, "changes": changes, "overflow": overflow}))
    }

    pub async fn next(&self, watch: Option<&str>) -> MixResult<IncomingEvent> {
        self.next_selected(watch, Families::ALL).await
    }

    /// A sleep yield point only consumes families with an actual handler.
    /// In particular, an unrelated handler cannot steal a later fs_wait batch.
    pub async fn next_selected(
        &self,
        watch: Option<&str>,
        families: Families,
    ) -> MixResult<IncomingEvent> {
        loop {
            let ready = self.ready.notified();
            tokio::pin!(ready);
            // Register BEFORE examining readiness, including notify_waiters cancellation.
            ready.as_mut().enable();
            {
                let mut p = self.pending.lock().unwrap();
                if p.closed {
                    return Err(refusal("NATIVE_CLOSED", "native sources retired"));
                }
                if let Some(h) = watch {
                    if !p.watches.contains_key(h) {
                        return Err(refusal("FS_WATCH_CANCELLED", "watch was removed"));
                    }
                    if let Some(body) = Self::take_watch(&mut p, h) {
                        return Ok(event("fs.changed", body));
                    }
                } else {
                    // Round-robin handles and rotate source families so a
                    // continuously written root (or a flapping link) cannot
                    // starve another watch or family.
                    let ready: Vec<_> = p
                        .watches
                        .iter()
                        .filter(|(_, w)| {
                            families.filesystem && (!w.changes.is_empty() || w.overflow)
                        })
                        .map(|(h, _)| h.clone())
                        .collect();
                    let h = ready
                        .iter()
                        .find(|h| p.last_watch.as_ref().is_none_or(|last| *h > last))
                        .or_else(|| ready.first())
                        .cloned();
                    let ready: Vec<_> = p
                        .sources
                        .iter()
                        .filter(|(_, s)| families.source(s.command) && s.ready())
                        .map(|(h, _)| h.clone())
                        .collect();
                    let s = ready
                        .iter()
                        .find(|h| p.last_source.as_ref().is_none_or(|last| *h > last))
                        .or_else(|| ready.first())
                        .cloned();
                    let child = families.children && !p.children.is_empty();
                    #[cfg(feature = "ws")]
                    let ready: Vec<_> = p
                        .sockets
                        .iter()
                        .filter(|(_, s)| {
                            families.sockets && (!s.frames.is_empty() || s.closed.is_some())
                        })
                        .map(|(h, _)| h.clone())
                        .collect();
                    #[cfg(feature = "ws")]
                    let so = ready
                        .iter()
                        .find(|h| p.last_socket.as_ref().is_none_or(|last| *h > last))
                        .or_else(|| ready.first())
                        .cloned();
                    let families_n: usize = if cfg!(feature = "ws") { 4 } else { 3 };
                    for step in 0..families_n {
                        let family = (p.turn + step) % families_n;
                        if family == 0
                            && let Some(h) = &h
                        {
                            p.last_watch = Some(h.clone());
                            p.turn = 1;
                            return Ok(event("fs.changed", Self::take_watch(&mut p, h).unwrap()));
                        }
                        if family == 1 && child {
                            p.turn = 2;
                            let body = p.children.pop_front().unwrap();
                            return Ok(event("proc.exited", body));
                        }
                        if family == 2
                            && let Some(s) = &s
                        {
                            p.last_source = Some(s.clone());
                            p.turn = 3;
                            return Ok(Self::take_source(&mut p, s).unwrap());
                        }
                        #[cfg(feature = "ws")]
                        if family == 3
                            && let Some(so) = &so
                        {
                            p.last_socket = Some(so.clone());
                            p.turn = 0;
                            return Ok(Self::take_socket(&mut p, so).unwrap());
                        }
                    }
                }
            }
            ready.await;
        }
    }
}

pub(crate) fn event(command: &str, body: serde_json::Value) -> IncomingEvent {
    IncomingEvent {
        command: command.into(),
        headers: BTreeMap::new(),
        body: body.to_string(),
    }
}

/// Native refusals use the existing exception path (nonzero execution RC), with
/// error_code/message available in the structured catch value.
pub(crate) fn refusal(code: &str, message: impl Into<String>) -> MixError {
    MixError::structured(code, message)
}

pub(crate) fn json_value(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(v) => Value::Bool(v),
        serde_json::Value::Number(v) => Value::Number(v.as_f64().unwrap_or_default()),
        serde_json::Value::String(v) => Value::String(v),
        serde_json::Value::Array(v) => Value::list(v.into_iter().map(json_value).collect()),
        serde_json::Value::Object(v) => {
            Value::map(v.into_iter().map(|(k, v)| (k, json_value(v))).collect())
        }
    }
}

/// Per-evaluator owner id for generation-scoped legacy child retirement
/// (`builtins::owned_spawns`): a serve hot-reload retires legacy
/// `die_with_parent` children by evaluator generation, and every evaluator —
/// including reload candidates — gets a fresh id.
static NEXT_OWNER_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct NativeEvents {
    pub queue: Arc<Queue>,
    pumping: Rc<Cell<bool>>,
    watches: BTreeSet<String>,
    filesystem: Option<crate::fs_watch::Registry>,
    children: Vec<crate::child_events::ChildWatch>,
    desktop: BTreeMap<String, crate::desktop_events::Source>,
    /// Subscribed sockets (ws_on/tcp_on), owned by this generation.
    #[cfg(feature = "ws")]
    pub(crate) sockets: BTreeMap<String, crate::builtins::socket_sources::SocketSource>,
    owner_id: u64,
}

impl Default for NativeEvents {
    fn default() -> Self {
        NativeEvents {
            queue: Arc::new(Queue::default()),
            pumping: Rc::new(Cell::new(false)),
            watches: BTreeSet::new(),
            filesystem: None,
            children: Vec::new(),
            desktop: BTreeMap::new(),
            #[cfg(feature = "ws")]
            sockets: BTreeMap::new(),
            owner_id: NEXT_OWNER_ID.fetch_add(1, Ordering::Relaxed),
        }
    }
}

impl NativeEvents {
    /// This evaluator generation's owner id (see [`NEXT_OWNER_ID`]).
    pub(crate) fn owner_id(&self) -> u64 {
        self.owner_id
    }

    pub fn enter_pump(&self) -> MixResult<PumpGuard> {
        if self.pumping.replace(true) {
            return Err(refusal(
                "NATIVE_CONSUMER",
                "this evaluator already has an event pump",
            ));
        }
        Ok(PumpGuard(self.pumping.clone()))
    }

    pub fn pumping(&self) -> bool {
        self.pumping.get()
    }
    pub fn has_sources(&mut self) -> bool {
        self.children.retain(|c| !c.finished());
        // Completed socket readers published their terminal record before
        // setting the flag; dropping them here joins an exited thread.
        // Undelivered records survive in the queue's pending slots.
        #[cfg(feature = "ws")]
        self.sockets
            .retain(|_, s| !s.completed.load(std::sync::atomic::Ordering::Acquire));
        !self.watches.is_empty()
            || !self.children.is_empty()
            || !self.desktop.is_empty()
            || !self.queue.pending.lock().unwrap().children.is_empty()
            || {
                #[cfg(feature = "ws")]
                {
                    !self.sockets.is_empty()
                        || !self.queue.pending.lock().unwrap().sockets.is_empty()
                }
                #[cfg(not(feature = "ws"))]
                {
                    false
                }
            }
    }

    /// `net_watch`: one rtnetlink subscription per handle.
    pub fn net_watch(&mut self, groups: u32) -> MixResult<String> {
        self.source_watch("net", "net.changed", move |queue, h| {
            crate::desktop_events::NetSource::new(queue, h, groups)
                .map(crate::desktop_events::Source::Net)
        })
    }

    /// `audio_watch`: one managed `pactl subscribe` child per handle.
    pub fn audio_watch(&mut self, opts: crate::desktop_events::AudioOptions) -> MixResult<String> {
        self.source_watch("audio", "audio.changed", move |queue, h| {
            crate::desktop_events::AudioSource::new(queue, h, opts)
                .map(crate::desktop_events::Source::Audio)
        })
    }

    fn source_watch(
        &mut self,
        family: &str,
        command: &'static str,
        start: impl FnOnce(Arc<Queue>, String) -> MixResult<crate::desktop_events::Source>,
    ) -> MixResult<String> {
        self.ensure_open()?;
        if self.desktop.len() >= crate::desktop_events::MAX_SOURCES {
            return Err(refusal(
                &format!("{}_WATCH_LIMIT", family.to_uppercase()),
                "maximum 16 net/audio watch handles per evaluator",
            ));
        }
        let h = format!("{family}:{}", NEXT_HANDLE.fetch_add(1, Ordering::Relaxed));
        // Register the pending slot first: the source may publish at once.
        self.queue.pending.lock().unwrap().sources.insert(
            h.clone(),
            PendingSource {
                command,
                ..Default::default()
            },
        );
        match start(self.queue.clone(), h.clone()) {
            Ok(source) => {
                self.desktop.insert(h.clone(), source);
                Ok(h)
            }
            Err(e) => {
                self.remove_source_pending(&h);
                Err(e)
            }
        }
    }

    fn remove_source_pending(&self, h: &str) {
        self.queue.pending.lock().unwrap().sources.remove(h);
        self.queue.ready.notify_waiters();
    }

    /// `net_unwatch` / `audio_unwatch`. A handle of the other family is
    /// refused, not cancelled.
    pub fn source_unwatch(&mut self, family: &str, h: &str) -> MixResult<()> {
        if !h.starts_with(&format!("{family}:")) || !self.desktop.contains_key(h) {
            return Err(refusal(
                &format!("{}_WATCH_HANDLE", family.to_uppercase()),
                "unknown or retired watch handle",
            ));
        }
        // Drop pending first: records the worker publishes while it is
        // being cancelled find no slot and are discarded.
        self.remove_source_pending(h);
        self.desktop.remove(h); // cancels and joins the worker
        Ok(())
    }
    pub fn watch(&mut self, path: &str, opts: crate::fs_watch::Options) -> MixResult<String> {
        self.ensure_open()?;
        if self.watches.len() >= MAX_WATCHES {
            return Err(refusal(
                "FS_WATCH_LIMIT",
                "maximum 128 watch handles per evaluator",
            ));
        }
        let h = format!("fs:{}", NEXT_HANDLE.fetch_add(1, Ordering::Relaxed));
        if self.filesystem.is_none() {
            self.filesystem = Some(crate::fs_watch::Registry::new(self.queue.clone())?);
        }
        self.queue
            .pending
            .lock()
            .unwrap()
            .watches
            .insert(h.clone(), PendingWatch::default());
        match self
            .filesystem
            .as_ref()
            .unwrap()
            .watch(path, opts, h.clone())
        {
            Ok(()) => {
                self.watches.insert(h.clone());
                Ok(h)
            }
            Err(e) => {
                self.remove_pending(&h);
                if self.watches.is_empty() {
                    self.filesystem = None;
                }
                Err(e)
            }
        }
    }

    fn remove_pending(&self, h: &str) {
        let mut p = self.queue.pending.lock().unwrap();
        if let Some(w) = p.watches.remove(h) {
            p.count -= w.changes.len();
        }
        drop(p);
        self.queue.ready.notify_waiters();
    }

    pub fn unwatch(&mut self, h: &str) -> MixResult<()> {
        if !self.watches.remove(h) {
            return Err(refusal(
                "FS_WATCH_HANDLE",
                "unknown or retired watch handle",
            ));
        }
        self.remove_pending(h); // rejects callbacks racing worker shutdown
        if let Some(filesystem) = &self.filesystem {
            filesystem.unwatch(h);
        }
        if self.watches.is_empty() {
            self.filesystem = None;
        }
        Ok(())
    }

    pub fn admit_child(&mut self) -> MixResult<()> {
        self.ensure_open()?;
        self.children.retain(|c| !c.finished());
        if self.children.len() + self.queue.pending.lock().unwrap().children.len() >= MAX_CHILDREN {
            return Err(refusal(
                "PROC_LIMIT",
                "maximum 128 managed children including pending exits",
            ));
        }
        Ok(())
    }

    fn ensure_open(&self) -> MixResult<()> {
        if self.queue.pending.lock().unwrap().closed {
            Err(refusal("NATIVE_CLOSED", "native sources retired"))
        } else {
            Ok(())
        }
    }

    pub fn own_child(&mut self, child: std::process::Child, tag: String) -> MixResult<u32> {
        let pid = child.id();
        self.children.push(crate::child_events::ChildWatch::new(
            child,
            tag,
            self.queue.clone(),
        )?);
        Ok(pid)
    }

    pub fn signal_child(&self, pid: u32, signal: i32) -> Option<bool> {
        // A freshly admitted child can reuse a reaped worker's PID before
        // that old worker finishes publishing its terminal record.
        if let Some(child) = self.children.iter().rev().find(|c| c.pid == pid) {
            return Some(child.signal(signal));
        }
        // A reaped child's event may still be queued after slot reclamation.
        self.queue
            .pending
            .lock()
            .unwrap()
            .children
            .iter()
            .any(|v| v["pid"].as_u64() == Some(pid as u64))
            .then_some(false)
    }

    /// `ws_on`: move a ws_connect handle into a subscription reader owned
    /// by this generation. The reader is the connection's single owner:
    /// numeric ws_recv/ws_close refuse from here on, while ws_send routes
    /// through the owner thread's command endpoint (registered by
    /// spawn_ws, removed when the source retires).
    #[cfg(feature = "ws")]
    pub fn ws_on(&mut self, client_id: u64, command: String) -> MixResult<String> {
        self.ensure_open()?;
        self.check_socket_limit()?;
        self.socket_sub("ws", crate::builtins::socket_sources::KIND_WS, command, |queue, id, command| {
            crate::builtins::socket_sources::subscribe_ws(queue, id, command, client_id)
        })
    }

    /// `tcp_on`: move a tcp_connect handle into a subscription reader.
    #[cfg(feature = "ws")]
    pub fn tcp_on(
        &mut self,
        client_id: u64,
        command: String,
        mode: crate::builtins::socket_sources::TcpMode,
    ) -> MixResult<String> {
        self.ensure_open()?;
        self.check_socket_limit()?;
        let kind = if mode.line {
            crate::builtins::socket_sources::KIND_TCP_LINE
        } else {
            crate::builtins::socket_sources::KIND_TCP_BYTES
        };
        self.socket_sub("tcp", kind, command, |queue, id, command| {
            crate::builtins::socket_sources::subscribe_tcp(queue, id, command, client_id, mode)
        })
    }

    #[cfg(feature = "ws")]
    fn check_socket_limit(&self) -> MixResult<()> {
        if self.sockets.len() >= crate::builtins::socket_sources::MAX_SOURCES {
            return Err(refusal(
                "SOCKET_LIMIT",
                "maximum 16 socket sources per evaluator",
            ));
        }
        Ok(())
    }

    /// Register the pending slot first (the worker may publish at once),
    /// then spawn the reader; a failed spawn leaves no slot behind.
    #[cfg(feature = "ws")]
    fn socket_sub(
        &mut self,
        family: &str,
        kind: &'static str,
        command: String,
        start: impl FnOnce(
            Arc<Queue>,
            String,
            String,
        ) -> MixResult<crate::builtins::socket_sources::SocketSource>,
    ) -> MixResult<String> {
        let h = format!("{family}:{}", NEXT_HANDLE.fetch_add(1, Ordering::Relaxed));
        self.queue.pending.lock().unwrap().sockets.insert(
            h.clone(),
            PendingSocket {
                command: command.clone(),
                kind,
                ..Default::default()
            },
        );
        match start(self.queue.clone(), h.clone(), command) {
            Ok(source) => {
                self.sockets.insert(h.clone(), source);
                Ok(h)
            }
            Err(e) => {
                self.remove_socket_pending(&h);
                Err(e)
            }
        }
    }

    /// Class C recv parking: at most one waiter per source. The guard
    /// clears the slot when the wait completes OR is cancelled (a Class C
    /// task abort drops the future, which drops the guard).
    #[cfg(feature = "ws")]
    pub fn park_socket(&mut self, h: &str) -> MixResult<ParkGuard> {
        {
            let mut p = self.queue.pending.lock().unwrap();
            let Some(s) = p.sockets.get_mut(h) else {
                return Err(refusal(
                    "SOCKET_WATCH_HANDLE",
                    "unknown or retired socket source",
                ));
            };
            if s.parked {
                return Err(refusal(
                    "SOCKET_BUSY",
                    "another recv is already waiting on this source",
                ));
            }
            s.parked = true;
        }
        Ok(ParkGuard {
            queue: self.queue.clone(),
            handle: h.to_string(),
        })
    }

    /// `ws_unwatch` / `tcp_unwatch`. Explicit close: cancel + join, drop
    /// queued frames, NO terminal marker (documented). A handle of the
    /// other family is refused, not cancelled.
    #[cfg(feature = "ws")]
    pub fn socket_unwatch(&mut self, family: &str, h: &str) -> MixResult<()> {
        let known = h.starts_with(&format!("{family}:"))
            && (self.sockets.contains_key(h)
                || self.queue.pending.lock().unwrap().sockets.contains_key(h));
        if !known {
            return Err(refusal(
                "SOCKET_WATCH_HANDLE",
                "unknown or retired socket source",
            ));
        }
        // Drop pending first: records the worker publishes while it is
        // being cancelled find no slot and are discarded; a parked recv
        // wakes with SOCKET_WATCH_HANDLE instead of hanging.
        self.remove_socket_pending(h);
        if let Some(source) = self.sockets.remove(h) {
            drop(source); // cancel byte + join
        }
        Ok(())
    }

    #[cfg(feature = "ws")]
    fn remove_socket_pending(&self, h: &str) {
        let mut p = self.queue.pending.lock().unwrap();
        if let Some(s) = p.sockets.remove(h) {
            p.socket_frames -= s.frames.len();
            p.socket_bytes -= s.bytes;
        }
        drop(p);
        self.queue.ready.notify_waiters();
    }

    pub fn close(&mut self) {
        {
            let mut p = self.queue.pending.lock().unwrap();
            p.closed = true;
            p.watches.clear();
            p.sources.clear();
            p.children.clear();
            p.count = 0;
            #[cfg(feature = "ws")]
            {
                p.sockets.clear();
                p.socket_frames = 0;
                p.socket_bytes = 0;
            }
        }
        self.queue.ready.notify_waiters();
        self.watches.clear();
        self.filesystem = None;
        self.children.clear();
        self.desktop.clear();
        #[cfg(feature = "ws")]
        self.sockets.clear(); // each Drop cancels + joins its reader
    }
}

impl Drop for NativeEvents {
    fn drop(&mut self) {
        self.close();
    }
}

/// Class C recv parking slot; Drop clears it so a cancelled wait never
/// wedges the source in the parked state.
#[cfg(feature = "ws")]
pub(crate) struct ParkGuard {
    queue: Arc<Queue>,
    handle: String,
}

#[cfg(feature = "ws")]
impl Drop for ParkGuard {
    fn drop(&mut self) {
        if let Some(s) = self.queue.pending.lock().unwrap().sockets.get_mut(&self.handle) {
            s.parked = false;
        }
    }
}

pub(crate) struct PumpGuard(Rc<Cell<bool>>);
impl Drop for PumpGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        sync::atomic::AtomicUsize,
        task::{Context, Poll, Wake, Waker},
    };

    #[derive(Default)]
    struct Wakes(AtomicUsize);
    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn queue() -> Arc<Queue> {
        let q = Arc::new(Queue::default());
        q.pending
            .lock()
            .unwrap()
            .watches
            .insert("test".into(), PendingWatch::default());
        q
    }
    fn change(path: String) -> Change {
        Change {
            path,
            kind: "modified",
            old_path: None,
        }
    }

    #[tokio::test]
    async fn bounded_coalescing_sticky_overflow_and_cancellation() {
        let q = queue();
        // Cancelling a waiting future must not consume a subsequent record.
        {
            let mut wait = pin!(q.next(Some("test")));
            let wakes = Arc::new(Wakes::default());
            let waker = Waker::from(wakes.clone());
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            assert_eq!(
                wakes.0.load(Ordering::Relaxed),
                0,
                "idle wait schedules no work"
            );
        }
        for n in 0..MAX_PENDING + 16 {
            q.change("test", Some(change(n.to_string())), false);
        }
        for _ in 0..5 {
            q.change("test", Some(change("0".into())), false);
        }
        let event = q.next(Some("test")).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(body["changes"].as_array().unwrap().len(), MAX_PENDING);
        assert_eq!(body["overflow"], true);
        assert_eq!(q.pending.lock().unwrap().count, 0);
        assert!(!q.pending.lock().unwrap().watches["test"].overflow);
    }

    #[tokio::test]
    async fn unwatch_wakes_pending_wait_and_rejects_late_callback() {
        let q = queue();
        let mut wait = pin!(q.next(Some("test")));
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        let registry = NativeEvents {
            queue: q.clone(),
            pumping: Rc::new(Cell::new(false)),
            watches: BTreeSet::new(),
            filesystem: None,
            children: Vec::new(),
            desktop: BTreeMap::new(),
            #[cfg(feature = "ws")]
            sockets: BTreeMap::new(),
            owner_id: 0,
        };
        registry.remove_pending("test");
        q.change("test", Some(change("late".into())), true);
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        assert_eq!(q.pending.lock().unwrap().count, 0);
    }

    #[tokio::test]
    async fn close_write_preserves_paired_move_endpoints() {
        let q = queue();
        q.change(
            "test",
            Some(Change {
                path: "new".into(),
                old_path: Some("old".into()),
                kind: "moved",
            }),
            false,
        );
        q.change("test", Some(change("new".into())), false);
        let event = q.next(None).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(v["changes"][0]["old_path"], "old");
        assert_eq!(v["changes"][0]["kind"], "moved");
    }

    #[tokio::test]
    async fn close_write_preserves_pending_creation() {
        let q = queue();
        q.change(
            "test",
            Some(Change {
                path: "new".into(),
                old_path: None,
                kind: "created",
            }),
            false,
        );
        q.change("test", Some(change("new".into())), false);
        let event = q.next(None).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&event.body).unwrap();
        assert_eq!(v["changes"][0]["kind"], "created");
    }

    #[tokio::test]
    async fn selecting_child_events_does_not_consume_a_waiters_filesystem_batch() {
        let q = queue();
        q.change("test", Some(change("kept".into())), false);
        q.child(serde_json::json!({"pid": 42}));
        let children = Families {
            children: true,
            ..Default::default()
        };
        let child = q.next_selected(None, children).await.unwrap();
        assert_eq!(child.command, "proc.exited");
        let batch = q.next(Some("test")).await.unwrap();
        assert!(batch.body.contains("kept"));
    }

    fn with_source(q: &Queue, handle: &str, command: &'static str) {
        q.pending.lock().unwrap().sources.insert(
            handle.into(),
            PendingSource {
                command,
                ..Default::default()
            },
        );
    }

    fn net(index: u32, up: bool) -> (String, serde_json::Value) {
        (
            format!("link:{index}"),
            serde_json::json!({"kind": "link", "index": index, "up": up}),
        )
    }

    #[tokio::test]
    async fn source_batches_coalesce_by_key_and_bound_with_sticky_overflow() {
        let q = queue();
        with_source(&q, "net:1", "net.changed");
        // A burst: link 3 flaps down then up; the batch holds the last word.
        q.source("net:1", vec![net(3, false), net(4, true)], false);
        q.source("net:1", vec![net(3, true)], false);
        let ev = q.next(None).await.unwrap();
        assert_eq!(ev.command, "net.changed");
        let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
        assert_eq!(body["watch"], "net:1");
        assert_eq!(body["overflow"], false);
        let changes = body["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|c| c["index"] == 3 && c["up"] == true));
        for n in 0..crate::desktop_events::MAX_SOURCE_PENDING as u32 + 5 {
            q.source("net:1", vec![net(n, true)], false);
        }
        let body: serde_json::Value =
            serde_json::from_str(&q.next(None).await.unwrap().body).unwrap();
        assert_eq!(
            body["changes"].as_array().unwrap().len(),
            crate::desktop_events::MAX_SOURCE_PENDING
        );
        assert_eq!(body["overflow"], true);
        // Overflow is reported once, then cleared.
        q.source("net:1", vec![net(1, true)], false);
        let body: serde_json::Value =
            serde_json::from_str(&q.next(None).await.unwrap().body).unwrap();
        assert_eq!(body["overflow"], false);
    }

    #[tokio::test]
    async fn closed_source_delivers_one_terminal_batch() {
        let q = queue();
        with_source(&q, "audio:1", "audio.changed");
        q.source_closed(
            "audio:1",
            serde_json::json!({"error_code": "AUDIO_SOURCE_EXITED"}),
        );
        let ev = q.next(None).await.unwrap();
        assert_eq!(ev.command, "audio.changed");
        let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
        assert_eq!(body["closed"]["error_code"], "AUDIO_SOURCE_EXITED");
        assert_eq!(body["overflow"], true);
        assert!(!q.pending.lock().unwrap().sources["audio:1"].ready());
    }

    #[tokio::test]
    async fn sleep_selection_leaves_unhandled_source_families_queued() {
        let q = queue();
        with_source(&q, "net:1", "net.changed");
        with_source(&q, "audio:2", "audio.changed");
        q.source("net:1", vec![net(3, true)], false);
        q.source("audio:2", vec![("sink#1".into(), serde_json::json!({}))], false);
        let audio_only = Families {
            audio: true,
            ..Default::default()
        };
        assert_eq!(
            q.next_selected(None, audio_only).await.unwrap().command,
            "audio.changed"
        );
        assert!(q.pending.lock().unwrap().sources["net:1"].ready());
        // With nothing selectable left, the wait pends instead of stealing.
        let mut wait = std::pin::pin!(q.next_selected(None, audio_only));
        let waker = Waker::from(Arc::new(Wakes::default()));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
    }

    #[tokio::test]
    async fn families_rotate_so_a_flapping_link_cannot_starve_filesystem_or_exits() {
        let q = queue();
        with_source(&q, "net:1", "net.changed");
        q.change("test", Some(change("a".into())), false);
        q.child(serde_json::json!({"pid": 1}));
        q.source("net:1", vec![net(3, true)], false);
        let mut seen = Vec::new();
        for _ in 0..3 {
            let ev = q.next(None).await.unwrap();
            seen.push(ev.command);
            // The link keeps flapping between deliveries.
            q.source("net:1", vec![net(3, true)], false);
        }
        seen.sort();
        assert_eq!(seen, ["fs.changed", "net.changed", "proc.exited"]);
    }

    #[tokio::test]
    async fn removing_a_source_wakes_waiters_and_drops_late_records() {
        let q = queue();
        with_source(&q, "net:1", "net.changed");
        let mut wait = std::pin::pin!(q.next(None));
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(wait.as_mut().poll(&mut cx).is_pending());
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0, "idle source schedules no work");
        // NativeEvents implements Drop, so no struct-update construction.
        let mut registry = NativeEvents::default();
        registry.queue = q.clone();
        registry.remove_source_pending("net:1");
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        q.source("net:1", vec![net(3, true)], true);
        q.source_closed("net:1", serde_json::json!({}));
        assert!(q.pending.lock().unwrap().sources.is_empty());
        assert!(wait.as_mut().poll(&mut cx).is_pending());
    }

    #[test]
    fn source_handles_are_family_checked_and_closed_registry_refuses() {
        let mut r = NativeEvents::default();
        let err = r.source_unwatch("net", "audio:1").unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "NET_WATCH_HANDLE"));
        let err = r.source_unwatch("audio", "audio:1").unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "AUDIO_WATCH_HANDLE"));
        r.close();
        let err = r
            .net_watch(crate::desktop_events::RTMGRP_LINK)
            .unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "NATIVE_CLOSED"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn net_watch_unwatch_and_close_join_their_workers() {
        let mut r = NativeEvents::default();
        let h = r.net_watch(crate::desktop_events::RTMGRP_LINK).unwrap();
        assert!(h.starts_with("net:"));
        assert!(r.has_sources());
        r.source_unwatch("net", &h).unwrap();
        assert!(!r.has_sources());
        assert!(r.queue.pending.lock().unwrap().sources.is_empty());
        let err = r.source_unwatch("net", &h).unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "NET_WATCH_HANDLE"));
        let mut handles = Vec::new();
        for _ in 0..crate::desktop_events::MAX_SOURCES {
            handles.push(r.net_watch(crate::desktop_events::RTMGRP_LINK).unwrap());
        }
        let err = r
            .net_watch(crate::desktop_events::RTMGRP_LINK)
            .unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "NET_WATCH_LIMIT"));
        // close() cancels every worker (Drop joins) and retires the slots.
        r.close();
        assert!(r.desktop.is_empty());
        assert!(r.queue.pending.lock().unwrap().sources.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn net_state_reports_loopback() {
        let v = crate::desktop_events::net_state().unwrap();
        let links = v["links"].as_array().unwrap();
        assert!(
            links
                .iter()
                .any(|l| l["loopback"] == true && l["ifname"].is_string())
        );
        assert!(v["addresses"].is_array());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn shared_directories_are_counted_once_and_released_after_last_handle() {
        let mut r = NativeEvents::default();
        let root = std::env::temp_dir().canonicalize().unwrap();
        let opts = || crate::fs_watch::Options::parse(None).unwrap();
        let first = r.watch(root.to_str().unwrap(), opts()).unwrap();
        let directories = r.queue.directories.load(Ordering::SeqCst);
        let second = r.watch(root.to_str().unwrap(), opts()).unwrap();
        assert_eq!(r.queue.directories.load(Ordering::SeqCst), directories);
        r.unwatch(&first).unwrap();
        assert_eq!(r.queue.directories.load(Ordering::SeqCst), directories);
        r.unwatch(&second).unwrap();
        assert_eq!(r.queue.directories.load(Ordering::SeqCst), 0);
        assert!(r.filesystem.is_none());
    }

    #[test]
    fn worker_boundary_is_owned_send_sync_data() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Queue>();
        send_sync::<Change>();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn directory_budget_refuses_without_leaking_a_registration() {
        let mut r = NativeEvents::default();
        r.queue.directories.store(MAX_DIRS, Ordering::SeqCst);
        let err = r
            .watch(
                std::env::temp_dir().to_str().unwrap(),
                crate::fs_watch::Options::parse(None).unwrap(),
            )
            .unwrap_err();
        assert!(matches!(err, MixError::Structured(info) if info.code == "FS_WATCH_LIMIT"));
        assert!(r.watches.is_empty());
        assert!(r.filesystem.is_none());
        assert!(r.queue.pending.lock().unwrap().watches.is_empty());
        r.queue.directories.store(0, Ordering::SeqCst);
    }

    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn kernel_overflow_survives_filter_and_rebuilds_watches() {
        let path = std::env::temp_dir().join(format!(
            "mix-overflow-{}-{}",
            std::process::id(),
            NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let mut r = NativeEvents::default();
        let h = r
            .watch(
                path.to_str().unwrap(),
                crate::fs_watch::Options {
                    recursive: true,
                    events: vec![],
                },
            )
            .unwrap();
        let other = r
            .watch(
                path.to_str().unwrap(),
                crate::fs_watch::Options {
                    recursive: true,
                    events: vec![],
                },
            )
            .unwrap();
        r.filesystem.as_ref().unwrap().inject(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan)
        ));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), r.queue.next(Some(&h)))
            .await
            .unwrap()
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
        assert_eq!(body["overflow"], true);
        assert!(body["changes"].as_array().unwrap().is_empty());
        let ev = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            r.queue.next(Some(&other)),
        )
        .await
        .unwrap()
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(&ev.body).unwrap();
        assert_eq!(body["overflow"], true);
        assert!(body["changes"].as_array().unwrap().is_empty());
        r.close();
        assert_eq!(r.queue.directories.load(Ordering::SeqCst), 0);
        std::fs::remove_dir(path).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn handle_or_kernel_limit_is_explicit_and_cleanup_releases_directories() {
        let path = std::env::temp_dir().join(format!(
            "mix-limits-{}-{}",
            std::process::id(),
            NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let mut r = NativeEvents::default();
        let mut refused = false;
        for _ in 0..=MAX_WATCHES {
            match r.watch(
                path.to_str().unwrap(),
                crate::fs_watch::Options::parse(None).unwrap(),
            ) {
                Ok(_) => assert!(r.watches.len() <= MAX_WATCHES),
                Err(e) => {
                    assert!(
                        matches!(e, MixError::Structured(info) if info.code == "FS_WATCH_LIMIT")
                    );
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        r.close();
        assert_eq!(r.queue.directories.load(Ordering::SeqCst), 0);
        std::fs::remove_dir(path).unwrap();
    }
}
