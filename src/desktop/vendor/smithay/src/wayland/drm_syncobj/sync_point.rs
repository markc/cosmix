use calloop::generic::Generic;
use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use drm::control::Device;
use std::sync::{Mutex, Weak};
use std::{
    io,
    os::unix::io::{AsFd, BorrowedFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::backend::drm::{DrmDeviceFd, WeakDrmDeviceFd};
use crate::backend::renderer::sync::{Fence, Interrupted};
use crate::wayland::compositor::{Blocker, BlockerState};

#[derive(Debug)]
pub(super) struct DrmTimelineInner {
    timeline_fd: OwnedFd,
    dev_ctx: Mutex<DrmTimelineDeviceSpecific>,
}

impl DrmTimelineInner {
    pub(super) fn update_device(&self, device: &DrmDeviceFd) -> io::Result<()> {
        let mut ctx = self.dev_ctx.lock().unwrap();
        let mut new = DrmTimelineDeviceSpecific::import(self.timeline_fd.as_fd(), device)?;
        for (point, eventfd) in ctx
            .event_fds
            .iter()
            .flat_map(|(p, fd)| fd.upgrade().map(|fd| (p, fd)))
        {
            device.syncobj_eventfd(new.syncobj, *point, eventfd.as_fd(), false)?;
            new.event_fds.push((*point, Arc::downgrade(&eventfd)));
        }
        *ctx = new;
        Ok(())
    }

    pub(super) fn invalidate(&self) {
        self.dev_ctx.lock().unwrap().invalidate()
    }
}

#[derive(Debug)]
struct DrmTimelineDeviceSpecific {
    device: WeakDrmDeviceFd,
    /// Weak handle to the DRM file itself, for destruction only.
    ///
    /// The imported syncobj handle lives in this file's handle table, not in
    /// the `DrmDeviceFd` that imported it, and the file outlives that
    /// `DrmDeviceFd` whenever a `DeviceFd` clone from `DrmDeviceFd::device_fd`
    /// is still held.
    device_file: Weak<OwnedFd>,
    syncobj: drm::control::syncobj::Handle,
    event_fds: Vec<(u64, Weak<OwnedFd>)>,
}

impl Drop for DrmTimelineDeviceSpecific {
    fn drop(&mut self) {
        self.destroy_syncobj_handle();
    }
}

impl DrmTimelineDeviceSpecific {
    fn import(fd: BorrowedFd<'_>, device: &DrmDeviceFd) -> io::Result<Self> {
        let syncobj = device.fd_to_syncobj(fd, false)?;
        Ok(DrmTimelineDeviceSpecific {
            device: device.downgrade(),
            device_file: device.device_fd().downgrade(),
            syncobj,
            event_fds: Vec::new(),
        })
    }

    /// Destroys the imported handle if the DRM file is still open.
    ///
    /// Keyed on the file rather than on `device` because the handle belongs to
    /// the file's handle table: it is still live, and still ours to release,
    /// after the last `DrmDeviceFd` is gone while a `DeviceFd` clone keeps the
    /// file open.
    ///
    /// A failed upgrade means every `DeviceFd` sharing this `Arc` is gone, which
    /// is not quite the same as the file being closed: a raw `dup` taken through
    /// the public `AsFd`/`AsRawFd` impls keeps the open file description, and so
    /// its handle table, alive with no `Arc` left to observe it. Nothing in this
    /// crate or in cosmix-comp duplicates the device fd that way, so every path
    /// that exists today does release the handle — but the type system does not
    /// enforce that, and if such a path is ever added this silently skips
    /// destruction and the handle lives until that descriptor closes.
    ///
    /// Two alternatives were considered and rejected, neither of them free.
    ///
    /// Holding the `Arc` strongly makes destruction unconditional, but pins the
    /// open file — and the kernel device state behind it, though not the
    /// `/dev/dri` entry — for as long as any imported timeline outlives the
    /// device, trading a leaked handle for a leaked file descriptor. Note this
    /// is not uniformly worse than the weak key: in the raw-`dup` case above the
    /// file is already held open by the duplicate, so strong retention would
    /// cost no extra lifetime there and would destroy the handle promptly. It
    /// loses in the common case and wins in the unreachable one, which is why
    /// the weak key is preferred rather than simply better.
    ///
    /// A handle registry on `DeviceFd`'s inner object — unregister on normal
    /// destruction, sweep whatever remains in the inner `Drop` before the fd
    /// closes — would close the `dup` gap without pinning anything. It is
    /// rejected on cost, not correctness: `DeviceFd` is used by the whole DRM
    /// backend, so this adds shared mutable state and locking to a core vendored
    /// type, re-applied at every version bump, to fix a case that no path in
    /// this crate or in cosmix-comp can currently reach. Revisit it if one ever
    /// can.
    ///
    /// All three beat the `device`-keyed destruction this replaced, which missed
    /// the `DeviceFd`-clone window as well.
    fn destroy_syncobj_handle(&self) {
        if let Some(file) = self.device_file.upgrade() {
            let _ = DrmFile(file).destroy_syncobj(self.syncobj);
        }
    }

    fn invalidate(&mut self) {
        self.destroy_syncobj_handle();
        self.device = WeakDrmDeviceFd::new();
        // Must be cleared alongside `device`: a later `Drop` that still
        // upgraded the file would destroy a handle id the kernel is free to
        // have reissued in the meantime.
        self.device_file = Weak::new();
        // trigger event fds
        for eventfd in self.event_fds.drain(..).filter_map(|(_, x)| Weak::upgrade(&x)) {
            // Known upstream defect: eventfd writes require 8 bytes; fixing this
            // would fail open by fabricating readiness, so leave the EINVAL no-op.
            let _ = rustix::io::write(&eventfd, &[1]);
        }
    }
}

/// DRM timeline syncobj
#[derive(Clone, Debug)]
pub struct DrmTimeline(pub(super) Arc<DrmTimelineInner>);

impl PartialEq for DrmTimeline {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl DrmTimeline {
    /// Import DRM timeline from file descriptor
    pub fn new(device: &DrmDeviceFd, fd: OwnedFd) -> io::Result<Self> {
        let dev_ctx = Mutex::new(DrmTimelineDeviceSpecific::import(fd.as_fd(), device)?);
        Ok(Self(Arc::new(DrmTimelineInner {
            timeline_fd: fd,
            dev_ctx,
        })))
    }

    /// Query the last signalled timeline point
    pub fn query_signalled_point(&self) -> io::Result<u64> {
        let ctx = self.0.dev_ctx.lock().unwrap();
        let device = ctx
            .device
            .upgrade()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?;

        let mut points = [0];
        device.syncobj_timeline_query(&[ctx.syncobj], &mut points, false)?;
        Ok(points[0])
    }
}

/// Point on a DRM timeline syncobj
#[derive(Clone, Debug)]
pub struct DrmSyncPoint {
    pub(super) timeline: DrmTimeline,
    pub(super) point: u64,
}

impl DrmSyncPoint {
    /// Create an eventfd that will be signaled by the syncpoint
    pub fn eventfd(&self) -> io::Result<Arc<OwnedFd>> {
        let fd = rustix::event::eventfd(
            0,
            rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
        )?;
        let mut ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .upgrade()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_eventfd(ctx.syncobj, self.point, fd.as_fd(), false)?;

        let fd = Arc::new(fd);
        ctx.event_fds.retain(|(_, fd)| fd.upgrade().is_some());
        ctx.event_fds.push((self.point, Arc::downgrade(&fd)));
        Ok(fd)
    }

    /// Signal the sync point.
    pub fn signal(&self) -> io::Result<()> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .upgrade()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_timeline_signal(&[ctx.syncobj], &[self.point])
    }

    /// Wait for sync point.
    pub fn wait(&self, timeout_nsec: i64) -> io::Result<()> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        ctx.device
            .upgrade()
            .ok_or::<io::Error>(io::ErrorKind::InvalidInput.into())?
            .syncobj_timeline_wait(&[ctx.syncobj], &[self.point], timeout_nsec, false, false, false)?;
        Ok(())
    }

    /// Export DRM sync file for sync point.
    pub fn export_sync_file(&self) -> io::Result<OwnedFd> {
        let ctx = self.timeline.0.dev_ctx.lock().unwrap();
        let Some(device) = ctx.device.upgrade() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };

        let syncobj = device.create_syncobj(false)?;
        if let Err(err) = device.syncobj_timeline_transfer(ctx.syncobj, syncobj, self.point, 0) {
            let _ = device.destroy_syncobj(syncobj);
            return Err(err);
        };
        let res = device.syncobj_to_fd(syncobj, true);
        if res.is_err() {
            let _ = device.destroy_syncobj(syncobj);
        }
        res
    }

    /// Create an [`calloop::EventSource`] and [`Blocker`] for this sync point.
    ///
    /// This will fail if `drmSyncobjEventfd` isn't supported by the device. See
    /// [`supports_syncobj_eventfd`](super::supports_syncobj_eventfd).
    pub fn generate_blocker(&self) -> io::Result<(DrmSyncPointBlocker, DrmSyncPointSource)> {
        let fd = self.eventfd()?;
        let signal = Arc::new(AtomicBool::new(false));
        let blocker = DrmSyncPointBlocker {
            signal: signal.clone(),
        };
        let source = DrmSyncPointSource {
            source: Generic::new(fd, Interest::READ, Mode::Level),
            signal,
        };
        Ok((blocker, source))
    }
}

impl Fence for DrmSyncPoint {
    fn is_signaled(&self) -> bool {
        self.timeline
            .query_signalled_point()
            .ok()
            .is_some_and(|point| point >= self.point)
    }

    fn wait(&self) -> Result<(), Interrupted> {
        self.wait(i64::MAX).map_err(|_| Interrupted)
    }

    fn is_exportable(&self) -> bool {
        true
    }

    fn export(&self) -> Option<OwnedFd> {
        self.export_sync_file().ok()
    }
}

/// Event source generating an event when a [`DrmSyncPoint`] is signalled..
#[derive(Debug)]
pub struct DrmSyncPointSource {
    source: Generic<Arc<OwnedFd>>,
    signal: Arc<AtomicBool>,
}

impl EventSource for DrmSyncPointSource {
    type Event = ();
    type Metadata = ();
    type Ret = Result<(), std::io::Error>;
    type Error = io::Error;

    fn process_events<C>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: C,
    ) -> Result<PostAction, Self::Error>
    where
        C: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        self.signal.store(true, Ordering::SeqCst);
        self.source
            .process_events(readiness, token, |_, _| Ok(PostAction::Remove))?;
        callback((), &mut ())?;
        Ok(PostAction::Remove)
    }

    fn register(&mut self, poll: &mut Poll, token_factory: &mut TokenFactory) -> calloop::Result<()> {
        self.source.register(poll, token_factory)?;
        Ok(())
    }

    fn reregister(&mut self, poll: &mut Poll, token_factory: &mut TokenFactory) -> calloop::Result<()> {
        self.source.reregister(poll, token_factory)?;
        Ok(())
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.source.unregister(poll)?;
        Ok(())
    }
}

/// [`Blocker`] implementation for an accompaning [`DrmSyncPointSource`]
#[derive(Debug)]
pub struct DrmSyncPointBlocker {
    signal: Arc<AtomicBool>,
}

impl Blocker for DrmSyncPointBlocker {
    fn state(&self) -> BlockerState {
        if self.signal.load(Ordering::SeqCst) {
            BlockerState::Released
        } else {
            BlockerState::Pending
        }
    }
}

/// Minimal `drm` control device over an already-open DRM file.
///
/// Local fix, appended at end of file so a version bump reapplies it without
/// landing inside an upstream struct/impl pair. See vendor/README.md.
///
/// Exists so destruction can issue `destroy_syncobj` holding only the file,
/// with no `DrmDeviceFd` and without extending the file's lifetime.
struct DrmFile(Arc<OwnedFd>);

impl AsFd for DrmFile {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for DrmFile {}
impl Device for DrmFile {}

// Local fix, appended at end of file so a version bump reapplies it without
// landing inside an upstream struct/impl pair. See vendor/README.md.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::DeviceFd;
    use std::{env, fs::File};

    #[test]
    #[ignore = "opens the DRM render node named by COSMIX_TEST_RENDER_NODE"]
    fn imported_syncobj_handles_are_destroyed_on_device_update_and_timeline_drop() {
        let render_node = env::var_os("COSMIX_TEST_RENDER_NODE")
            .expect("COSMIX_TEST_RENDER_NODE must name a DRM render node");
        let file = File::options()
            .read(true)
            .write(true)
            .open(&render_node)
            .expect("open DRM render node");
        let device = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));

        let exported_handle = device.create_syncobj(false).expect("create timeline syncobj");
        let timeline_fd = device
            .syncobj_to_fd(exported_handle, false)
            .expect("export timeline syncobj to fd");
        device
            .destroy_syncobj(exported_handle)
            .expect("destroy exported timeline handle");

        let timeline = DrmTimeline::new(&device, timeline_fd).expect("import timeline syncobj");
        let h1 = timeline.0.dev_ctx.lock().unwrap().syncobj;

        // `update_device` imports the replacement before dropping the old
        // context, so the two handles are necessarily distinct -- this does not
        // depend on the kernel's handle-reuse behaviour.
        timeline.0.update_device(&device).expect("update timeline device");
        let h2 = timeline.0.dev_ctx.lock().unwrap().syncobj;
        assert_ne!(h1, h2);
        assert_destroyed(&device, h1, "replaced imported handle");

        drop(timeline);
        assert_destroyed(&device, h2, "final imported handle");
    }

    // Probes with a timeline query rather than an export: an export allocates a
    // file descriptor, so under a low `RLIMIT_NOFILE` it fails with EMFILE
    // whether or not the handle was destroyed, and a bare `is_err()` would then
    // pass on a build with the leak still present. A query allocates no
    // descriptor.
    //
    // Accepts ENOENT alone, and deliberately not "any error". The kernel takes
    // this path with flags 0 and one handle, so EINVAL cannot come from the
    // handle lookup -- it means a malformed request, which would fail
    // identically on a leaking build and hand back a false green. Everything
    // else a caller could hit here (ENOMEM from the kernel's handle array,
    // EOPNOTSUPP on a driver without timeline syncobjs, EFAULT, ENOTTY) is
    // likewise not evidence of destruction, so all of it fails loudly. If some
    // kernel ever reports a missing handle differently, this goes red rather
    // than silently green, which is the safe direction for an oracle.
    // Proves the destruction path is keyed on the DRM *file*, not on the
    // `DrmDeviceFd` that imported the handle. The other test cannot: it holds a
    // live `DrmDeviceFd` throughout, so a `self.device.upgrade()` destruction
    // would pass it just as well. Here the last `DrmDeviceFd` is dropped first
    // and only a bare `DeviceFd` clone keeps the file open, which is exactly
    // the case where upgrading `device` fails and the handle would be stranded
    // for as long as that clone lives.
    #[test]
    #[ignore = "opens the DRM render node named by COSMIX_TEST_RENDER_NODE"]
    fn imported_handle_is_destroyed_when_only_a_device_fd_clone_keeps_the_file_open() {
        let render_node = env::var_os("COSMIX_TEST_RENDER_NODE")
            .expect("COSMIX_TEST_RENDER_NODE must name a DRM render node");
        let file = File::options()
            .read(true)
            .write(true)
            .open(&render_node)
            .expect("open DRM render node");
        let device = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(file)));

        let exported_handle = device.create_syncobj(false).expect("create timeline syncobj");
        let timeline_fd = device
            .syncobj_to_fd(exported_handle, false)
            .expect("export timeline syncobj to fd");
        device
            .destroy_syncobj(exported_handle)
            .expect("destroy exported timeline handle");

        let timeline = DrmTimeline::new(&device, timeline_fd).expect("import timeline syncobj");
        let handle = timeline.0.dev_ctx.lock().unwrap().syncobj;

        // Keep the file open by a route that does not keep the `DrmDeviceFd`
        // alive, then drop the last `DrmDeviceFd`.
        let keepalive = device.device_fd();
        drop(device);

        drop(timeline);

        // Probe through the file that is still open, since the device this
        // handle was imported through no longer exists.
        let probe = DrmFile(
            keepalive
                .downgrade()
                .upgrade()
                .expect("the retained DeviceFd clone keeps the file open"),
        );
        assert_destroyed(&probe, handle, "handle behind a surviving DeviceFd clone");
    }

    fn assert_destroyed(device: &impl Device, handle: drm::control::syncobj::Handle, what: &str) {
        let mut points = [0u64];
        let err = device
            .syncobj_timeline_query(&[handle], &mut points, false)
            .expect_err(&format!("{what} must be destroyed"));
        assert_eq!(
            err.raw_os_error(),
            Some(rustix::io::Errno::NOENT.raw_os_error()),
            "{what}: expected ENOENT from the handle lookup, got {err:?}"
        );
    }
}
