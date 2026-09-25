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
//! Implementation: notify's callback only forwards into a channel; one plain
//! thread (blocking `recv`, so event-driven) applies events under the table
//! lock. It never calls back into the notify event loop from that loop's own
//! thread. Any event on a watched directory's own path re-checks the
//! directory's inode, so a missed or coalesced self-event still re-arms.
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
    warned: bool,
}

/// The shared watch service. `add`/`remove`/`rebind` are called by the router
/// (the only writer of path bindings); events arrive on the watch thread.
#[derive(Clone)]
pub struct Watch {
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
    /// Start the watcher and its event thread. A watcher that cannot be
    /// created (no inotify) leaves every buffer `unwatched`.
    pub fn start() -> Watch {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
        match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(watcher) => inner.lock().expect("watch table").watcher = Some(watcher),
            Err(error) => tracing::warn!("cosmix-editd: no file watcher ({error}); every buffer is unwatched"),
        }
        let thread_inner = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("editd-watch".into())
            .spawn(move || {
                // Ends when the watcher (the only sender) is dropped.
                while let Ok(res) = rx.recv() {
                    let Some(inner) = thread_inner.upgrade() else { break };
                    let mut guard = inner.lock().expect("watch table");
                    match res {
                        Ok(event) => guard.handle(&event),
                        Err(error) => tracing::warn!("cosmix-editd: watch error: {error}"),
                    }
                }
            })
            .expect("spawn the watch thread");
        Watch { inner }
    }

    /// Bind `bid` to `file`: watch its parent directory (refcounted).
    pub fn add(&self, bid: &str, file: &Path, signal: Arc<DiskSignal>) {
        let mut inner = self.inner.lock().expect("watch table");
        inner.bind(bid, file, signal);
    }

    /// Unbind `bid`; the directory watch goes when its last buffer does.
    pub fn remove(&self, bid: &str) {
        let mut inner = self.inner.lock().expect("watch table");
        inner.unbind(bid);
    }

    /// Move `bid`'s binding to `file` (save-as), keeping its signal.
    pub fn rebind(&self, bid: &str, file: &Path) {
        let mut inner = self.inner.lock().expect("watch table");
        if let Some(bound) = inner.files.get(bid) {
            let signal = bound.signal.clone();
            inner.unbind(bid);
            inner.bind(bid, file, signal);
        }
    }

    /// Directories currently watched on themselves (tests, `info`).
    pub fn armed_dirs(&self) -> Vec<PathBuf> {
        self.inner.lock().expect("watch table").armed.keys().cloned().collect()
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
            match self.watch(dir) {
                Ok(()) => {
                    self.armed.insert(dir.to_path_buf(), id);
                    self.stop_waiting(dir);
                    self.signal_dir(dir, |s| {
                        s.set_unwatched(false);
                        s.raise();
                    });
                }
                Err(error) => {
                    if !self.warned {
                        self.warned = true;
                        tracing::warn!("cosmix-editd: cannot watch {} ({error}); buffers there are unwatched", dir.display());
                    }
                    self.signal_dir(dir, |s| s.set_unwatched(true));
                }
            }
            return;
        }
        // The directory is gone: its files are deleted; wait on an ancestor.
        self.signal_dir(dir, DiskSignal::raise);
        let mut anc = dir.parent();
        for _ in 0..WATCH_ANCESTOR_DEPTH {
            let Some(a) = anc else { break };
            if dir_id(a).is_some() {
                let a = a.to_path_buf();
                if self.ancestors.get(&a).is_some_and(|t| t.contains(dir)) {
                    return;
                }
                let fresh = !self.ancestors.contains_key(&a) && !self.armed.contains_key(&a);
                if !fresh || self.watch(&a).is_ok() {
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
