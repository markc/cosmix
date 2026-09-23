//! Directory inotify sources owned by the existing layer-host calloop loop.
//! No timer, worker thread, debounce or content comparison is involved.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::Resource;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken, generic::Generic};
use rustix::fs::inotify;

/// Application watches queued before the host starts its event loop.
#[derive(Resource, Default)]
pub struct LayerHostFileWatches(pub Vec<LayerHostFileWatch>);

/// Watches the containing directory, keeping atomic file replacement and
/// deletion/re-creation observable. Install before the initial file read.
pub struct LayerHostFileWatch {
    fd: Arc<OwnedFd>,
    path: PathBuf,
    changed: Arc<dyn Fn() + Send + Sync>,
}

impl LayerHostFileWatch {
    pub fn new(path: PathBuf, changed: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| io::Error::other("file watch requires a containing directory"))?;
        std::fs::create_dir_all(parent)?;
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        inotify::add_watch(
            &fd,
            parent,
            inotify::WatchFlags::MODIFY
                | inotify::WatchFlags::CLOSE_WRITE
                | inotify::WatchFlags::CREATE
                | inotify::WatchFlags::MOVED_TO
                | inotify::WatchFlags::MOVED_FROM
                | inotify::WatchFlags::DELETE
                | inotify::WatchFlags::DELETE_SELF
                | inotify::WatchFlags::MOVE_SELF
                | inotify::WatchFlags::ONLYDIR,
        )?;
        Ok(Self {
            fd: Arc::new(fd),
            path,
            changed,
        })
    }

    /// Drain each kernel event separately, calling the reader immediately.
    /// Public so headless hosts/tests can dispatch already-ready events without
    /// installing a Wayland host. This does not wait or schedule periodic reads.
    /// Inotify does not retain overwritten file bytes; callers must not promise
    /// historical snapshots for edits completed before event dispatch.
    pub fn dispatch_pending(&self) -> io::Result<usize> {
        let mut buffer = [MaybeUninit::uninit(); 4096];
        let mut events = inotify::Reader::new(self.fd.as_ref(), &mut buffer);
        let mut count = 0;
        loop {
            let event = match events.next() {
                Ok(event) => event,
                Err(rustix::io::Errno::AGAIN) => return Ok(count),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            };
            let flags = event.events();
            if flags.intersects(
                inotify::ReadFlags::QUEUE_OVERFLOW
                    | inotify::ReadFlags::IGNORED
                    | inotify::ReadFlags::DELETE_SELF
                    | inotify::ReadFlags::MOVE_SELF,
            ) {
                // Loss of the directory or queue contents must never silently
                // leave a supposedly live watcher behind.
                return Err(io::Error::other(format!(
                    "file watch invalidated: {} ({flags:?})",
                    self.path.display()
                )));
            }
            if event.file_name().is_some_and(|name| {
                self.path
                    .file_name()
                    .is_some_and(|target| name.to_bytes() == target.as_bytes())
            }) {
                (self.changed)();
                count += 1;
            }
        }
    }

    pub fn insert<S: 'static>(
        self,
        handle: &LoopHandle<'_, S>,
        mut wake: impl FnMut(&mut S) + 'static,
    ) -> Result<RegistrationToken, String> {
        let source = Generic::new(Arc::clone(&self.fd), Interest::READ, Mode::Level);
        handle
            .insert_source(source, move |_, _, state| match self.dispatch_pending() {
                Ok(count) => {
                    if count != 0 {
                        wake(state);
                    }
                    Ok(PostAction::Continue)
                }
                Err(error) => {
                    tracing::error!("file watch stopped: {error}");
                    wake(state);
                    Ok(PostAction::Remove)
                }
            })
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn directory_watch_dispatches_on_host_event_loop() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("quoin-watch-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("conf.mix");
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let watch = LayerHostFileWatch::new(
            path.clone(),
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        let mut event_loop = calloop::EventLoop::<bool>::try_new().unwrap();
        watch
            .insert(&event_loop.handle(), |wake| *wake = true)
            .unwrap();
        for source in ["{}", "{bad: true}"] {
            let replacement = directory.join("replacement.mix");
            std::fs::write(&replacement, source).unwrap();
            std::fs::rename(replacement, &path).unwrap();
            let mut woke = false;
            event_loop
                .dispatch(Duration::from_secs(2), &mut woke)
                .unwrap();
            assert!(woke);
        }
        assert!(calls.load(Ordering::SeqCst) >= 2);
        let before = calls.load(Ordering::SeqCst);
        let unrelated = directory.join("unrelated.mix");
        std::fs::write(&unrelated, "{}").unwrap();
        let mut woke = false;
        event_loop
            .dispatch(Duration::from_secs(2), &mut woke)
            .unwrap();
        assert!(!woke);
        assert_eq!(calls.load(Ordering::SeqCst), before);
        drop(event_loop);
        std::fs::remove_file(unrelated).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
