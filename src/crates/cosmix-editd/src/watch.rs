//! External-change detection (plan §4.7). Event-driven; no polling.
//!
//! One `notify::RecommendedWatcher` (inotify) watches each open file's PARENT
//! directory non-recursively, refcounted per directory (a file watch would lose
//! track after another editor's rename-over-save). Events set the owning
//! actor's "recheck disk" flag.
//!
//! On recheck the actor stats the bound path: equal to `base` → nothing (this
//! swallows our own saves); otherwise hash — equal to `base` → refresh `base`;
//! different → clean buffer: `reload_minimal` as `tool:disk`, `disk: clean`;
//! dirty: `disk: modified`; gone: `disk: deleted`. Each transition emits a
//! `disk` event and `props.changed`.
//!
//! # Contract: re-registration (frozen)
//! inotify watches a directory INODE. On the watched directory's own removal
//! or move (`IN_DELETE_SELF` / `IN_MOVE_SELF`, surfaced by notify as a remove
//! or rename of the watched path itself) and the `IN_IGNORED` that follows:
//! drop the dead watch, re-resolve the parent BY PATH, and
//! - parent exists → add a new watch, recheck every actor bound under it;
//! - parent missing → mark those buffers `disk: deleted`, watch the nearest
//!   existing ancestor (non-recursive, at most `WATCH_ANCESTOR_DEPTH` levels),
//!   re-arm one level down on each matching create until the parent is back,
//!   then recheck.
//!
//! Watch failure (inotify limit, network fs) → warn once, `disk: unwatched`;
//! the save-time revalidation still guards saves.
//!
//! An ANCESTOR watch is an inode watch too: its own replacement (a rename
//! away, a delete) is detected the same way — the ancestor's inode is recorded
//! when it is armed, any event on its own path (and every rescan) re-checks
//! it, and a changed inode drops that watch and re-arms every target that was
//! waiting under it from the top (nearest existing ancestor again).
//!
//! Implementation: notify's callback only forwards into a channel; one plain
//! thread (blocking `recv`, so event-driven) owns the table and applies both
//! notify events and the router's bind/unbind/rebind requests, in order. The
//! router only SENDS a request — the filesystem work of arming (metadata,
//! inotify registration) never runs on the router's task. The thread never
//! calls back into the notify event loop from that loop's own thread. Any
//! event on a watched directory's own path re-checks the directory's inode,
//! so a missed or coalesced self-event still re-arms.
//! Access events (open, read-close) are ignored: the actor's own hashing reads
//! must not wake it again.

use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cosmix_edit_core::wire::BufferId;
use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecursiveMode, Watcher};
use tokio::sync::Notify;

use crate::limits::WATCH_ANCESTOR_DEPTH;

/// Directory → buffers bound under it. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct WatchTable {
    pub dirs: HashMap<PathBuf, Vec<BufferId>>,
}

/// The watcher → actor doorbell: a coalescing "recheck disk" flag plus the
/// `unwatched` state. A burst of events costs the actor one `stat`.
#[derive(Debug, Default)]
pub struct DiskSignal {
    recheck: AtomicBool,
    unwatched: AtomicBool,
    pub notify: Notify,
}

impl DiskSignal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Ask the actor to recheck (idempotent until taken).
    pub fn raise(&self) {
        self.recheck.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// Consume a pending recheck.
    pub fn take(&self) -> bool {
        self.recheck.swap(false, Ordering::AcqRel)
    }

    pub fn set_unwatched(&self, unwatched: bool) {
        if self.unwatched.swap(unwatched, Ordering::AcqRel) != unwatched {
            self.raise();
        }
    }

    pub fn is_unwatched(&self) -> bool {
        self.unwatched.load(Ordering::Acquire)
    }
}

struct Bound {
    file: PathBuf,
    signal: Arc<DiskSignal>,
}

#[derive(Default)]
struct Inner {
    watcher: Option<notify::RecommendedWatcher>,
    table: WatchTable,
    files: HashMap<BufferId, Bound>,
    /// Directories with a live watch on themselves, with the inode watched.
    armed: HashMap<PathBuf, (u64, u64)>,
    /// Ancestor directory → missing target directories waiting under it.
    ancestors: HashMap<PathBuf, BTreeSet<PathBuf>>,
    /// The inode each ancestor watch was armed on.
    ancestor_ids: HashMap<PathBuf, (u64, u64)>,
    warned: bool,
    /// Test hook: runs between `rearm`'s existence check and its watch call.
    #[cfg(test)]
    before_watch: Option<Box<dyn FnMut(&Path) + Send>>,
}

/// Work for the watch thread, in arrival order.
enum Msg {
    Event(notify::Result<notify::Event>),
    Bind { bid: BufferId, file: PathBuf, signal: Arc<DiskSignal> },
    Unbind { bid: BufferId },
    Rebind { bid: BufferId, file: PathBuf },
    /// Answered once everything sent before it is applied (tests, `armed_dirs`).
    Sync(std::sync::mpsc::Sender<()>),
}

/// The shared watch service. `add`/`remove`/`rebind` are called by the router
/// (the only writer of path bindings) and only enqueue: the watch thread
/// applies them, interleaved in order with the watcher's events.
#[derive(Clone)]
pub struct Watch {
    tx: std::sync::mpsc::Sender<Msg>,
    inner: Arc<Mutex<Inner>>,
}

fn dir_id(dir: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(dir).ok().filter(|m| m.is_dir()).map(|m| (m.dev(), m.ino()))
}

fn relevant(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read))
    )
}

impl Watch {
    /// Start the watcher and its thread. A watcher that cannot be created (no
    /// inotify) leaves every buffer `unwatched`.
    pub fn start() -> Watch {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let events = tx.clone();
        match notify::recommended_watcher(move |res| {
            let _ = events.send(Msg::Event(res));
        }) {
            Ok(watcher) => inner.lock().expect("watch table").watcher = Some(watcher),
            Err(error) => tracing::warn!("cosmix-editd: no file watcher ({error}); every buffer is unwatched"),
        }
        let thread_inner = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("editd-watch".into())
            .spawn(move || {
                // Ends when every sender is gone: the `Watch` handles and the
                // watcher (owned by `Inner`, dropped with the last handle).
                while let Ok(msg) = rx.recv() {
                    let Some(inner) = thread_inner.upgrade() else { break };
                    let mut guard = inner.lock().expect("watch table");
                    match msg {
                        Msg::Event(Ok(event)) => guard.handle(&event),
                        Msg::Event(Err(error)) => tracing::warn!("cosmix-editd: watch error: {error}"),
                        Msg::Bind { bid, file, signal } => guard.bind(&bid, &file, signal),
                        Msg::Unbind { bid } => guard.unbind(&bid),
                        Msg::Rebind { bid, file } => {
                            if let Some(bound) = guard.files.get(&bid) {
                                let signal = bound.signal.clone();
                                guard.unbind(&bid);
                                guard.bind(&bid, &file, signal);
                            }
                        }
                        Msg::Sync(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("spawn the watch thread");
        Watch { tx, inner }
    }

    /// Bind `bid` to `file`: watch its parent directory (refcounted).
    pub fn add(&self, bid: &str, file: &Path, signal: Arc<DiskSignal>) {
        let _ = self.tx.send(Msg::Bind { bid: bid.to_string(), file: file.to_path_buf(), signal });
    }

    /// Unbind `bid`; the directory watch goes when its last buffer does.
    pub fn remove(&self, bid: &str) {
        let _ = self.tx.send(Msg::Unbind { bid: bid.to_string() });
    }

    /// Move `bid`'s binding to `file` (save-as), keeping its signal.
    pub fn rebind(&self, bid: &str, file: &Path) {
        let _ = self.tx.send(Msg::Rebind { bid: bid.to_string(), file: file.to_path_buf() });
    }

    /// Wait until every request sent so far has been applied (blocking; tests).
    pub fn sync(&self) {
        let (done, wait) = std::sync::mpsc::channel();
        if self.tx.send(Msg::Sync(done)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// Directories currently watched on themselves (tests, `info`).
    pub fn armed_dirs(&self) -> Vec<PathBuf> {
        self.sync();
        self.inner.lock().expect("watch table").armed.keys().cloned().collect()
    }

    /// Ancestor directories currently watched for missing targets (tests).
    pub fn ancestor_dirs(&self) -> Vec<PathBuf> {
        self.sync();
        self.inner.lock().expect("watch table").ancestors.keys().cloned().collect()
    }
}

impl Inner {
    fn bind(&mut self, bid: &str, file: &Path, signal: Arc<DiskSignal>) {
        let Some(dir) = file.parent().map(Path::to_path_buf) else {
            signal.set_unwatched(true);
            return;
        };
        self.files.insert(bid.to_string(), Bound { file: file.to_path_buf(), signal });
        let bids = self.table.dirs.entry(dir.clone()).or_default();
        let first = bids.is_empty();
        bids.push(bid.to_string());
        if first && !self.armed.contains_key(&dir) {
            self.rearm(&dir);
        } else if !self.armed.contains_key(&dir) && !self.waiting(&dir) {
            // Previously failed outright (unwatched): the new buffer shares that state.
            self.signal_dir(&dir, |s| s.set_unwatched(true));
        }
    }

    fn unbind(&mut self, bid: &str) {
        let Some(bound) = self.files.remove(bid) else { return };
        let Some(dir) = bound.file.parent().map(Path::to_path_buf) else { return };
        let empty = match self.table.dirs.get_mut(&dir) {
            Some(bids) => {
                bids.retain(|b| b != bid);
                bids.is_empty()
            }
            None => false,
        };
        if empty {
            self.table.dirs.remove(&dir);
            if self.armed.remove(&dir).is_some() && !self.ancestors.contains_key(&dir) {
                self.unwatch(&dir);
            }
            self.stop_waiting(&dir);
        }
    }

    fn waiting(&self, dir: &Path) -> bool {
        self.ancestors.values().any(|targets| targets.contains(dir))
    }

    fn stop_waiting(&mut self, target: &Path) {
        let ancestors: Vec<PathBuf> =
            self.ancestors.iter().filter(|(_, t)| t.contains(target)).map(|(a, _)| a.clone()).collect();
        for anc in ancestors {
            if let Some(targets) = self.ancestors.get_mut(&anc) {
                targets.remove(target);
                if targets.is_empty() {
                    self.ancestors.remove(&anc);
                    self.ancestor_ids.remove(&anc);
                    if !self.armed.contains_key(&anc) {
                        self.unwatch(&anc);
                    }
                }
            }
        }
    }

    fn watch(&mut self, dir: &Path) -> notify::Result<()> {
        match self.watcher.as_mut() {
            Some(w) => w.watch(dir, RecursiveMode::NonRecursive),
            None => Err(notify::Error::generic("no watcher")),
        }
    }

    fn unwatch(&mut self, dir: &Path) {
        if let Some(w) = self.watcher.as_mut() {
            let _ = w.unwatch(dir);
        }
    }

    fn signal_dir(&self, dir: &Path, f: impl Fn(&DiskSignal)) {
        if let Some(bids) = self.table.dirs.get(dir) {
            for bid in bids {
                if let Some(bound) = self.files.get(bid) {
                    f(&bound.signal);
                }
            }
        }
    }

    /// Watch `dir` by path if it exists, else the nearest existing ancestor.
    fn rearm(&mut self, dir: &Path) {
        if let Some(id) = dir_id(dir) {
            #[cfg(test)]
            if let Some(hook) = self.before_watch.as_mut() {
                hook(dir);
            }
            match self.watch(dir) {
                Ok(()) => {
                    // The inode actually watched: the path may have been
                    // replaced since `id` was read.
                    let id = dir_id(dir).unwrap_or(id);
                    self.armed.insert(dir.to_path_buf(), id);
                    self.stop_waiting(dir);
                    self.signal_dir(dir, |s| {
                        s.set_unwatched(false);
                        s.raise();
                    });
                    return;
                }
                // Gone between the check and the watch (a rename or delete
                // racing the arm): not a watch failure — wait on an ancestor
                // like any missing directory, below. Marking it unwatched
                // here left nothing armed to see the directory come back.
                Err(_) if dir_id(dir).is_none() => {}
                Err(error) => {
                    if !self.warned {
                        self.warned = true;
                        tracing::warn!("cosmix-editd: cannot watch {} ({error}); buffers there are unwatched", dir.display());
                    }
                    self.signal_dir(dir, |s| s.set_unwatched(true));
                    return;
                }
            }
        }
        // The directory is gone: its files are deleted; wait on an ancestor.
        self.signal_dir(dir, DiskSignal::raise);
        let mut anc = dir.parent();
        for _ in 0..WATCH_ANCESTOR_DEPTH {
            let Some(a) = anc else { break };
            if let Some(id) = dir_id(a) {
                let a = a.to_path_buf();
                if self.ancestor_ids.get(&a).is_some_and(|recorded| *recorded != id) {
                    // A stale ancestor watch whose replacement was not seen
                    // yet: never join it (it watches the old inode).
                    self.check_ancestor(&a);
                }
                if self.ancestors.get(&a).is_some_and(|t| t.contains(dir)) {
                    return;
                }
                let fresh = !self.ancestors.contains_key(&a) && !self.armed.contains_key(&a);
                if !fresh || self.watch(&a).is_ok() {
                    // The inode this watch sees: its replacement is detected
                    // by `check_ancestor`.
                    self.ancestor_ids.entry(a.clone()).or_insert(id);
                    self.ancestors.entry(a.clone()).or_default().insert(dir.to_path_buf());
                    // Watch, THEN look: the next level may have appeared
                    // before the ancestor watch existed, and would never
                    // produce the event we are waiting for.
                    let next = dir.ancestors().take_while(|p| *p != a).last();
                    if next.is_some_and(|n| dir_id(n).is_some()) {
                        self.stop_waiting(dir);
                        self.rearm(dir);
                    }
                    return;
                }
                break;
            }
            anc = a.parent();
        }
        self.signal_dir(dir, |s| s.set_unwatched(true));
    }

    fn handle(&mut self, event: &notify::Event) {
        if event.need_rescan() {
            for bound in self.files.values() {
                bound.signal.raise();
            }
            let dirs: Vec<PathBuf> = self.armed.keys().cloned().collect();
            for dir in dirs {
                self.check_dir(&dir);
            }
            let ancestors: Vec<PathBuf> = self.ancestors.keys().cloned().collect();
            for anc in ancestors {
                self.check_ancestor(&anc);
            }
            return;
        }
        if !relevant(&event.kind) {
            return;
        }
        for path in &event.paths {
            for bound in self.files.values() {
                if &bound.file == path {
                    bound.signal.raise();
                }
            }
            if self.armed.contains_key(path) {
                self.check_dir(path);
            }
            if self.ancestors.contains_key(path) {
                self.check_ancestor(path);
            }
            let hit: Vec<PathBuf> = path
                .parent()
                .and_then(|parent| self.ancestors.get(parent))
                .map(|targets| targets.iter().filter(|t| t.starts_with(path)).cloned().collect())
                .unwrap_or_default();
            for target in hit {
                self.stop_waiting(&target);
                self.rearm(&target);
            }
        }
    }

    /// The watched directory's path may no longer be the inode we watch.
    fn check_dir(&mut self, dir: &Path) {
        let watched = self.armed.get(dir).copied();
        if watched.is_some() && dir_id(dir) == watched {
            return;
        }
        self.armed.remove(dir);
        self.unwatch(dir);
        self.signal_dir(dir, DiskSignal::raise);
        self.rearm(dir);
    }

    /// An ancestor watch's path may no longer be the inode it watches (moved
    /// away, deleted): drop it and re-arm every target waiting under it from
    /// scratch, which finds the nearest existing ancestor again.
    fn check_ancestor(&mut self, anc: &Path) {
        let Some(recorded) = self.ancestor_ids.get(anc).copied() else { return };
        if dir_id(anc) == Some(recorded) {
            return;
        }
        let targets = self.ancestors.remove(anc).unwrap_or_default();
        self.ancestor_ids.remove(anc);
        if !self.armed.contains_key(anc) {
            self.unwatch(anc);
        }
        for target in targets {
            self.rearm(&target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn raised(signal: &DiskSignal) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let notified = signal.notify.notified();
            if signal.take() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return signal.take();
            }
        }
    }

    #[tokio::test]
    async fn write_to_a_bound_file_raises_its_signal() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let watch = Watch::start();
        let signal = DiskSignal::new();
        watch.add("b1_00000000", &file, signal.clone());
        assert!(raised(&signal).await, "arming raises a first recheck");
        std::fs::write(&file, "two").unwrap();
        assert!(raised(&signal).await, "external write detected");
        assert!(!signal.is_unwatched());
    }

    #[tokio::test]
    async fn replaced_parent_directory_rearms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let dir = root.join("sub");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let watch = Watch::start();
        let signal = DiskSignal::new();
        watch.add("b1_00000000", &file, signal.clone());
        assert!(raised(&signal).await);

        // Rename the parent away: the buffer rechecks (file gone).
        std::fs::rename(&dir, root.join("old")).unwrap();
        assert!(raised(&signal).await, "moving the parent away raises a recheck");

        // A new directory moves into place: re-armed and rechecked.
        let fresh = root.join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(fresh.join("a.txt"), "two").unwrap();
        std::fs::rename(&fresh, &dir).unwrap();
        // Re-arming raises the signal after recording the new inode, so each
        // wake is a doorbell to re-read the table (no sleep loop).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while dir_id(&dir) != watch.inner.lock().unwrap().armed.get(&dir).copied() {
            let woke = tokio::time::timeout_at(deadline, signal.notify.notified()).await;
            assert!(woke.is_ok(), "watch never re-armed on the new inode");
        }

        // A later external write through the new directory is detected.
        let _ = signal.take();
        std::fs::write(&file, "three").unwrap();
        assert!(raised(&signal).await, "writes after re-arming are detected");
    }

    /// The flaky `replaced_parent_directory_rearms_through_dispatch` race,
    /// made deterministic: the directory is renamed away between `rearm`'s
    /// existence check and its inotify registration. It must fall back to
    /// the ancestor wait (not `unwatched` with nothing armed), so its return
    /// is still seen.
    #[tokio::test]
    async fn directory_vanishing_mid_arm_waits_on_an_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let dir = root.join("sub");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one").unwrap();
        let watch = Watch::start();
        let away = root.join("old");
        let mut fired = false;
        watch.inner.lock().unwrap().before_watch = Some(Box::new(move |d: &Path| {
            if !fired && d.ends_with("sub") {
                fired = true;
                std::fs::rename(d, &away).unwrap();
            }
        }));
        let signal = DiskSignal::new();
        watch.add("b1_00000000", &file, signal.clone());
        assert!(raised(&signal).await, "the buffer rechecks (its file is gone)");
        assert_eq!(watch.ancestor_dirs(), vec![root.clone()], "waiting on the parent, not given up");
        assert!(!signal.is_unwatched());

        let fresh = root.join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        std::fs::write(fresh.join("a.txt"), "two").unwrap();
        std::fs::rename(&fresh, &dir).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while dir_id(&dir) != watch.inner.lock().unwrap().armed.get(&dir).copied() {
            let woke = tokio::time::timeout_at(deadline, signal.notify.notified()).await;
            assert!(woke.is_ok(), "the returned directory was never armed");
        }
        let _ = signal.take();
        std::fs::write(&file, "three").unwrap();
        assert!(raised(&signal).await, "writes after re-arming are detected");
    }

    #[tokio::test]
    async fn replaced_ancestor_watch_rearms() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let a = root.join("a");
        let b = a.join("b");
        std::fs::create_dir_all(&b).unwrap();
        let file = b.join("f.txt");
        std::fs::write(&file, "one").unwrap();
        let watch = Watch::start();
        let signal = DiskSignal::new();
        watch.add("b1_00000000", &file, signal.clone());
        assert!(raised(&signal).await);

        // Delete a/b: the buffer waits on the ancestor `a`.
        std::fs::remove_file(&file).unwrap();
        std::fs::remove_dir(&b).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !watch.ancestor_dirs().contains(&a) {
            let woke = tokio::time::timeout_at(deadline, signal.notify.notified()).await;
            assert!(woke.is_ok(), "never fell back to the ancestor");
        }

        // Rename the ANCESTOR away and rebuild a/b/f.txt under a new `a`: the
        // stale ancestor watch must be replaced, and the target re-armed.
        std::fs::rename(&a, root.join("old-a")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(&file, "two").unwrap();
        while dir_id(&b).is_none() || dir_id(&b) != watch.inner.lock().unwrap().armed.get(&b).copied() {
            let woke = tokio::time::timeout_at(deadline, signal.notify.notified()).await;
            assert!(woke.is_ok(), "a replaced ancestor never re-armed its target");
        }
        let _ = signal.take();
        std::fs::write(&file, "three").unwrap();
        assert!(raised(&signal).await, "writes after re-arming are detected");
    }

    #[tokio::test]
    async fn refcounted_directory_watch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let watch = Watch::start();
        watch.add("b1_00000000", &dir.join("a"), DiskSignal::new());
        watch.add("b2_00000000", &dir.join("b"), DiskSignal::new());
        assert_eq!(watch.armed_dirs(), vec![dir.clone()]);
        watch.remove("b1_00000000");
        assert_eq!(watch.armed_dirs(), vec![dir.clone()]);
        watch.remove("b2_00000000");
        assert!(watch.armed_dirs().is_empty());
    }
}
