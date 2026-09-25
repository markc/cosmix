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

#[derive(Default)]
struct Pending {
    watches: BTreeMap<String, PendingWatch>,
    children: VecDeque<serde_json::Value>,
    count: usize,
    closed: bool,
    last_watch: Option<String>,
    prefer_child: bool,
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
        self.next_selected(watch, true, true).await
    }

    /// A sleep yield point only consumes families with an actual handler.
    /// In particular, an unrelated handler cannot steal a later fs_wait batch.
    pub async fn next_selected(
        &self,
        watch: Option<&str>,
        filesystem: bool,
        children: bool,
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
                    // Round-robin handles and alternate source families so a
                    // continuously written root cannot starve another watch.
                    let ready: Vec<_> = p
                        .watches
                        .iter()
                        .filter(|(_, w)| filesystem && (!w.changes.is_empty() || w.overflow))
                        .map(|(h, _)| h.clone())
                        .collect();
                    let h = ready
                        .iter()
                        .find(|h| p.last_watch.as_ref().is_none_or(|last| *h > last))
                        .or_else(|| ready.first())
                        .cloned();
                    if children
                        && (p.prefer_child || h.is_none())
                        && let Some(body) = p.children.pop_front()
                    {
                        p.prefer_child = false;
                        return Ok(event("proc.exited", body));
                    }
                    if let Some(h) = h {
                        p.last_watch = Some(h.clone());
                        p.prefer_child = true;
                        return Ok(event("fs.changed", Self::take_watch(&mut p, &h).unwrap()));
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
            || !self.queue.pending.lock().unwrap().children.is_empty()
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
            p.children.clear();
            p.count = 0;
        }
        self.queue.ready.notify_waiters();
        self.watches.clear();
        self.filesystem = None;
        self.children.clear();
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
        let child = q.next_selected(None, false, true).await.unwrap();
        assert_eq!(child.command, "proc.exited");
        let batch = q.next(Some("test")).await.unwrap();
        assert!(batch.body.contains("kept"));
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
