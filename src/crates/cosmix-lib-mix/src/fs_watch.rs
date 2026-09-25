//! Linux inotify only. Each registration watches its stable parent and manually
//! enumerated directories; no symlink following and no periodic reconciliation.
use crate::{error::MixResult, native_events::refusal, value::Value};

pub(crate) struct Options {
    pub recursive: bool,
    pub events: Vec<String>,
}

impl Options {
    pub fn parse(value: Option<&Value>) -> MixResult<Self> {
        let mut opts = Self {
            recursive: false,
            events: ["created", "modified", "deleted", "moved"]
                .map(str::to_owned)
                .to_vec(),
        };
        let Some(value) = value else { return Ok(opts) };
        let Value::Map(map) = value else {
            return Err(refusal("FS_WATCH_OPTIONS", "options must be a map"));
        };
        for (key, value) in map.iter() {
            match (key.as_str(), value) {
                ("recursive", Value::Bool(v)) => opts.recursive = *v,
                ("events", Value::List(values)) => {
                    opts.events.clear();
                    for v in values.iter() {
                        let Value::String(s) = v else {
                            return Err(refusal(
                                "FS_WATCH_OPTIONS",
                                "events must be a list of event names",
                            ));
                        };
                        if !["created", "modified", "deleted", "moved"].contains(&s.as_str()) {
                            return Err(refusal(
                                "FS_WATCH_OPTIONS",
                                format!("unknown event kind: {s}"),
                            ));
                        }
                        if !opts.events.contains(s) {
                            opts.events.push(s.clone());
                        }
                    }
                }
                _ => {
                    return Err(refusal(
                        "FS_WATCH_OPTIONS",
                        format!("invalid watch option: {key}"),
                    ));
                }
            }
        }
        Ok(opts)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::native_events::{Change, MAX_DIRS, Queue};
    use notify::{
        EventKind, Watcher,
        event::{AccessKind, AccessMode, ModifyKind, RenameMode},
    };
    use std::{
        cell::RefCell,
        collections::{BTreeMap, BTreeSet},
        path::{Path, PathBuf},
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    type WatchReply = mpsc::SyncSender<Result<(), (String, String)>>;

    enum Input {
        Event(notify::Result<notify::Event>),
        Add(String, Options, String, WatchReply),
        Remove(String, mpsc::SyncSender<()>),
        Stop,
    }

    /// One inotify instance/event-loop thread and one reconciliation worker
    /// shared by all handles in this evaluator generation.
    pub(crate) struct Registry {
        tx: mpsc::SyncSender<Input>,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    fn io_error(e: std::io::Error) -> crate::MixError {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => "FS_WATCH_MISSING",
            std::io::ErrorKind::PermissionDenied => "FS_WATCH_PERMISSION",
            _ => "FS_WATCH_IO",
        };
        refusal(code, e.to_string())
    }

    fn notify_error(e: notify::Error) -> crate::MixError {
        let code = match &e.kind {
            notify::ErrorKind::MaxFilesWatch => "FS_WATCH_LIMIT",
            notify::ErrorKind::PathNotFound => "FS_WATCH_MISSING",
            notify::ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                "FS_WATCH_PERMISSION"
            }
            notify::ErrorKind::Io(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOSPC | libc::EMFILE | libc::ENFILE)
                ) =>
            {
                "FS_WATCH_LIMIT"
            }
            _ => "FS_WATCH_IO",
        };
        refusal(code, e.to_string())
    }

    struct State {
        watcher: Rc<RefCell<Directories>>,
        dirs: BTreeSet<PathBuf>,
        root: PathBuf,
        parent: PathBuf,
        directory: bool,
        opts: Options,
        queue: Arc<Queue>,
        handle: String,
    }

    struct Directories {
        watcher: notify::INotifyWatcher,
        refs: BTreeMap<PathBuf, usize>,
        queue: Arc<Queue>,
    }

    impl Directories {
        fn add(&mut self, path: &Path) -> MixResult<()> {
            if let Some(count) = self.refs.get_mut(path) {
                *count += 1;
                return Ok(());
            }
            self.queue
                .directories
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n < MAX_DIRS).then_some(n + 1)
                })
                .map_err(|_| {
                    refusal(
                        "FS_WATCH_LIMIT",
                        "maximum 8192 directory watches per evaluator",
                    )
                })?;
            if let Err(e) = self
                .watcher
                .watch(path, notify::RecursiveMode::NonRecursive)
            {
                self.queue.directories.fetch_sub(1, Ordering::SeqCst);
                return Err(notify_error(e));
            }
            self.refs.insert(path.into(), 1);
            Ok(())
        }

        fn remove(&mut self, path: &Path) {
            let Some(count) = self.refs.get_mut(path) else {
                return;
            };
            *count -= 1;
            if *count == 0 {
                self.refs.remove(path);
                let _ = self.watcher.unwatch(path);
                self.queue.directories.fetch_sub(1, Ordering::SeqCst);
            }
        }

        fn refresh(&mut self, path: &Path) -> MixResult<()> {
            // Reinstall even when another handle retains this pathname: the
            // kernel may have forgotten its old inode after a missed delete.
            let _ = self.watcher.unwatch(path);
            self.watcher
                .watch(path, notify::RecursiveMode::NonRecursive)
                .map_err(notify_error)
        }
    }

    impl State {
        fn add(&mut self, path: &Path) -> MixResult<()> {
            if self.dirs.contains(path) {
                return Ok(());
            }
            self.watcher.borrow_mut().add(path)?;
            self.dirs.insert(path.to_owned());
            Ok(())
        }

        fn reconcile(&mut self) -> MixResult<()> {
            let mut wanted = BTreeSet::from([self.parent.clone()]);
            let mut todo = vec![self.root.clone()];
            while let Some(path) = todo.pop() {
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_error(e)),
                };
                if !self.directory || !meta.is_dir() || meta.file_type().is_symlink() {
                    continue;
                }
                if wanted.len() >= MAX_DIRS && !wanted.contains(&path) {
                    return Err(refusal(
                        "FS_WATCH_LIMIT",
                        "directory enumeration exceeds 8192",
                    ));
                }
                wanted.insert(path.clone());
                // Install before enumerating: later children generate inotify events.
                self.add(&path)?;
                if self.opts.recursive {
                    for entry in std::fs::read_dir(&path).map_err(io_error)? {
                        let entry = entry.map_err(io_error)?;
                        if entry.file_type().map_err(io_error)?.is_dir() {
                            if todo.len() + wanted.len() >= MAX_DIRS {
                                return Err(refusal(
                                    "FS_WATCH_LIMIT",
                                    "directory enumeration exceeds 8192",
                                ));
                            }
                            todo.push(entry.path());
                        }
                    }
                }
            }
            let stale: Vec<_> = self.dirs.difference(&wanted).cloned().collect();
            for path in stale {
                self.watcher.borrow_mut().remove(&path);
                self.dirs.remove(&path);
            }
            Ok(())
        }

        fn relevant(&self, path: &Path) -> bool {
            path == self.root
                || (self.directory
                    && if self.opts.recursive {
                        path.starts_with(&self.root)
                    } else {
                        path.parent() == Some(self.root.as_path())
                    })
        }

        fn record(&self, path: &Path, kind: &'static str, old: Option<&Path>) {
            if !self.relevant(path) && !old.is_some_and(|p| self.relevant(p)) {
                return;
            }
            if !self.opts.events.iter().any(|e| e == kind) {
                return;
            }
            // Invalid UTF-8 cannot be faithfully represented by Mix strings: rescan.
            let Some(path) = path.to_str() else {
                self.overflow();
                return;
            };
            if old.is_some_and(|p| p.to_str().is_none()) {
                self.overflow();
                return;
            }
            self.queue.change(
                &self.handle,
                Some(Change {
                    path: path.into(),
                    kind,
                    old_path: old.map(|p| p.to_str().unwrap().into()),
                }),
                false,
            );
        }

        fn overflow(&self) {
            self.queue.change(&self.handle, None, true);
        }

        fn recover(&mut self) -> MixResult<()> {
            // Lost delete/rename records can leave a path in our index after
            // the kernel removed its inode watch. Rebuild registrations, not
            // just the desired path set, before claiming ongoing coverage.
            for path in std::mem::take(&mut self.dirs) {
                let mut watcher = self.watcher.borrow_mut();
                watcher.remove(&path);
                if watcher.refs.contains_key(&path) {
                    let _ = watcher.refresh(&path);
                }
            }
            self.add(&self.parent.clone())?;
            self.reconcile()
        }

        fn event(&mut self, event: &notify::Result<notify::Event>) {
            let event = match event {
                Ok(e) => e,
                Err(_) => {
                    let _ = self.recover();
                    self.overflow();
                    return;
                }
            };
            let rescan = event.need_rescan();
            if !rescan
                && !event.paths.is_empty()
                && !event
                    .paths
                    .iter()
                    .any(|p| self.relevant(p) || self.root.starts_with(p))
            {
                return;
            }
            if rescan {
                let _ = self.recover();
                self.overflow();
            }
            let structural = matches!(
                event.kind,
                EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Modify(ModifyKind::Name(_))
            );
            // inotify forgets a deleted inode. Retire our path index too, before
            // watching its replacement (or the same inode under its new name).
            let removed = match event.kind {
                EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Name(RenameMode::From | RenameMode::Both)) => {
                    event.paths.first()
                }
                _ => None,
            };
            if let Some(path) = removed {
                let stale: Vec<_> = self
                    .dirs
                    .iter()
                    .filter(|d| **d != self.parent && d.starts_with(path))
                    .cloned()
                    .collect();
                for d in stale {
                    let mut watcher = self.watcher.borrow_mut();
                    watcher.remove(&d);
                    if watcher.refs.contains_key(&d) {
                        let _ = watcher.refresh(&d);
                    }
                    self.dirs.remove(&d);
                }
            }
            if structural || rescan {
                let before = self.dirs.clone();
                if self.reconcile().is_err() || self.dirs.difference(&before).next().is_some() {
                    // Files may have appeared before a new directory's watch installed.
                    self.overflow();
                }
            }
            if matches!(
                event.kind,
                EventKind::Modify(ModifyKind::Name(RenameMode::Both))
            ) && event.paths.len() == 2
            {
                self.record(&event.paths[1], "moved", Some(&event.paths[0]));
            } else {
                let kind = match event.kind {
                    EventKind::Create(_) => Some("created"),
                    EventKind::Remove(_) => Some("deleted"),
                    // A half rename is an invalidation of this endpoint, never a guess.
                    EventKind::Modify(ModifyKind::Name(_)) => Some("moved"),
                    EventKind::Modify(_)
                    | EventKind::Access(AccessKind::Close(AccessMode::Write)) => Some("modified"),
                    EventKind::Any | EventKind::Other => {
                        self.overflow();
                        None
                    }
                    _ => None,
                };
                if let Some(kind) = kind {
                    for p in &event.paths {
                        self.record(p, kind, None);
                    }
                }
            }
        }
    }

    impl Drop for State {
        fn drop(&mut self) {
            for path in &self.dirs {
                self.watcher.borrow_mut().remove(path);
            }
        }
    }

    impl State {
        fn new(
            path: &str,
            opts: Options,
            handle: String,
            queue: Arc<Queue>,
            watcher: Rc<RefCell<Directories>>,
        ) -> MixResult<Self> {
            let path = Path::new(path);
            let meta = std::fs::symlink_metadata(path).map_err(io_error)?;
            if meta.file_type().is_symlink() {
                return Err(refusal(
                    "FS_WATCH_SYMLINK",
                    "watch root must not be a symlink",
                ));
            }
            // Canonicalise the parent only; the leaf identity is its directory entry.
            let root = path.canonicalize().map_err(io_error)?;
            let parent = root.parent().unwrap_or(&root).to_owned();
            let mut state = Self {
                watcher,
                dirs: BTreeSet::new(),
                root,
                parent: parent.clone(),
                directory: meta.is_dir(),
                opts,
                queue,
                handle,
            };
            state.add(&parent)?;
            state.reconcile()?;
            Ok(state)
        }
    }

    impl Registry {
        pub fn new(queue: Arc<Queue>) -> MixResult<Self> {
            let (tx, rx) = mpsc::sync_channel(256);
            let lost = Arc::new(AtomicBool::new(false));
            let callback_tx = tx.clone();
            let callback_lost = lost.clone();
            let callback_queue = queue.clone();
            let watcher = notify::INotifyWatcher::new(
                move |e: notify::Result<notify::Event>| {
                    // Enumeration produces open/read-close notices. They are
                    // not changes and must not fill the bounded queue while
                    // recovery enumerates a large tree (a feedback loop).
                    if let Ok(event) = &e
                        && matches!(event.kind, EventKind::Access(_))
                        && !matches!(
                            event.kind,
                            EventKind::Access(AccessKind::Close(AccessMode::Write))
                        )
                        && !event.need_rescan()
                    {
                        return;
                    }
                    if let Err(mpsc::TrySendError::Full(_)) = callback_tx.try_send(Input::Event(e))
                    {
                        callback_lost.store(true, Ordering::Release);
                        callback_queue.overflow_watches();
                        // If the worker drained the queue between try_send and the
                        // flag store, leave a fresh wake so it reconciles the gap.
                        let _ = callback_tx
                            .try_send(Input::Event(Ok(notify::Event::new(EventKind::Any))));
                    }
                },
                notify::Config::default(),
            )
            .map_err(notify_error)?;
            let stopped = Arc::new(AtomicBool::new(false));
            let worker_stopped = stopped.clone();
            let worker = thread::Builder::new()
                .name("mix-inotify".into())
                .spawn(move || {
                    let watcher = Rc::new(RefCell::new(Directories {
                        watcher,
                        refs: BTreeMap::new(),
                        queue: queue.clone(),
                    }));
                    let mut states = BTreeMap::<String, State>::new();
                    while let Ok(input) = rx.recv() {
                        if worker_stopped.load(Ordering::Acquire) {
                            break;
                        }
                        match input {
                            Input::Stop => break,
                            Input::Event(e) => {
                                for state in states.values_mut() {
                                    state.event(&e);
                                }
                            }
                            Input::Add(path, opts, handle, reply) => {
                                let result = State::new(
                                    &path,
                                    opts,
                                    handle.clone(),
                                    queue.clone(),
                                    watcher.clone(),
                                )
                                .map(|state| {
                                    states.insert(handle, state);
                                })
                                .map_err(|e| match e {
                                    crate::MixError::Structured(info) => (info.code, info.message),
                                    error => ("FS_WATCH_IO".into(), error.to_string()),
                                });
                                let _ = reply.send(result);
                            }
                            Input::Remove(handle, reply) => {
                                states.remove(&handle);
                                let _ = reply.send(());
                            }
                        }
                        if lost.swap(false, Ordering::AcqRel) {
                            for state in states.values_mut() {
                                let _ = state.recover();
                                // Report AFTER repair, even if the earlier hint
                                // was already consumed by the evaluator.
                                state.overflow();
                            }
                        }
                    }
                })
                .map_err(io_error)?;
            Ok(Self {
                tx,
                stopped,
                worker: Some(worker),
            })
        }

        pub fn watch(&self, path: &str, opts: Options, handle: String) -> MixResult<()> {
            let (tx, rx) = mpsc::sync_channel(1);
            self.tx
                .send(Input::Add(path.into(), opts, handle, tx))
                .map_err(|_| refusal("FS_WATCH_IO", "watch worker stopped"))?;
            rx.recv()
                .map_err(|_| refusal("FS_WATCH_IO", "watch worker stopped"))?
                // MixError can carry thread-local Values. Only transfer the
                // owned code/message across the worker boundary.
                .map_err(|(code, message)| refusal(&code, message))
        }

        pub fn unwatch(&self, handle: &str) {
            let (tx, rx) = mpsc::sync_channel(1);
            if self.tx.send(Input::Remove(handle.into(), tx)).is_ok() {
                let _ = rx.recv();
            }
        }

        #[cfg(test)]
        pub fn inject(&self, event: notify::Result<notify::Event>) {
            self.tx.send(Input::Event(event)).unwrap();
        }
    }

    impl Drop for Registry {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            // A full queue already supplies the worker's wake. Cancellation
            // does not wait behind the event backlog or compete for capacity.
            let _ = self.tx.try_send(Input::Stop);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::Registry;

#[cfg(not(target_os = "linux"))]
pub(crate) struct Registry;
#[cfg(not(target_os = "linux"))]
impl Registry {
    pub fn new(_: std::sync::Arc<crate::native_events::Queue>) -> MixResult<Self> {
        Err(refusal(
            "FS_WATCH_UNSUPPORTED",
            "fs_watch requires Linux inotify; there is no polling fallback",
        ))
    }
    pub fn watch(&self, _: &str, _: Options, _: String) -> MixResult<()> {
        Err(refusal(
            "FS_WATCH_UNSUPPORTED",
            "fs_watch requires Linux inotify; there is no polling fallback",
        ))
    }
    pub fn unwatch(&self, _: &str) {}
}
