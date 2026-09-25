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

#[derive(Default)]
struct Pending {
    watches: BTreeMap<String, PendingWatch>,
    sources: BTreeMap<String, PendingSource>,
    children: VecDeque<serde_json::Value>,
    count: usize,
    closed: bool,
    last_watch: Option<String>,
    last_source: Option<String>,
    /// Round-robin over filesystem (0), child (1) and net/audio (2) records.
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
}

impl Families {
    pub const ALL: Self = Self {
        filesystem: true,
        children: true,
        net: true,
        audio: true,
    };

    fn source(self, command: &str) -> bool {
        match command {
            "net.changed" => self.net,
            "audio.changed" => self.audio,
            _ => false,
        }
    }
}

#[derive(Default)]
pub(crate) struct Queue {
    pending: Mutex<Pending>,
    ready: Notify,
    pub directories: AtomicUsize,
}

impl Queue {
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
                // Do not erase a paired rename when a close-write follows it.
                if c.kind == "modified" && previous.kind == "moved" {
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
                    for step in 0..3 {
                        let family = (p.turn + step) % 3;
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
                            p.turn = 0;
                            return Ok(Self::take_source(&mut p, s).unwrap());
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

#[derive(Default)]
pub(crate) struct NativeEvents {
    pub queue: Arc<Queue>,
    pumping: Rc<Cell<bool>>,
    watches: BTreeSet<String>,
    filesystem: Option<crate::fs_watch::Registry>,
    children: Vec<crate::child_events::ChildWatch>,
    desktop: BTreeMap<String, crate::desktop_events::Source>,
}

impl NativeEvents {
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
        !self.watches.is_empty()
            || !self.children.is_empty()
            || !self.desktop.is_empty()
            || !self.queue.pending.lock().unwrap().children.is_empty()
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

    pub fn close(&mut self) {
        {
            let mut p = self.queue.pending.lock().unwrap();
            p.closed = true;
            p.watches.clear();
            p.sources.clear();
            p.children.clear();
            p.count = 0;
        }
        self.queue.ready.notify_waiters();
        self.watches.clear();
        self.filesystem = None;
        self.children.clear();
        self.desktop.clear();
    }
}

impl Drop for NativeEvents {
    fn drop(&mut self) {
        self.close();
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
