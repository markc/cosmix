//! Compositor-owned raw atomic-KMS presentation.
//!
//! Every request and event correlation is built in this module. Smithay supplies
//! GBM buffers; drm-rs supplies property/blob/AddFB helpers. Its modeset,
//! event-reader and `clear_state` paths are not reachable from this platform.

#![cfg_attr(test, allow(dead_code))]

use std::{
    collections::{BTreeMap, VecDeque},
    fmt, io,
    num::NonZeroU32,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use smithay::reexports::drm::{
    Device as BasicDrmDevice,
    buffer::PlanarBuffer,
    control::{self, Device as ControlDevice, ResourceHandle},
};

use super::{
    kms::AtomicOutputSelection,
    render::{CancelScope, PresentDeadline, PresentOutcome, PresentationCancelHandle},
    scan::connector_mode,
    scanout_pool::{ScanoutPool, ScanoutSlotId, ScanoutSlotState},
    worker::KmsRenderPlatformFailure,
};

const ATOMIC_PRESENT_TIMEOUT_CODE: &str = "kms-live-atomic-present-deadline-expired";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicPropertyIds {
    connector_crtc_id: u32,
    crtc_active: u32,
    crtc_mode_id: u32,
    plane_fb_id: u32,
    plane_crtc_id: u32,
    plane_crtc_x: u32,
    plane_crtc_y: u32,
    plane_crtc_w: u32,
    plane_crtc_h: u32,
    plane_src_x: u32,
    plane_src_y: u32,
    plane_src_w: u32,
    plane_src_h: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AtomicProperty {
    object: u32,
    property: u32,
    value: u64,
}

/// The compositor's backend-independent representation of an atomic request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AtomicRequest {
    properties: Vec<AtomicProperty>,
}

impl AtomicRequest {
    fn set(&mut self, object: u32, property: u32, value: u64) {
        self.properties.push(AtomicProperty {
            object,
            property,
            value,
        });
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicCommitOptions {
    pub(crate) test_only: bool,
    pub(crate) allow_modeset: bool,
    pub(crate) nonblock: bool,
    pub(crate) page_flip_event: bool,
    pub(crate) correlation: Option<AtomicCommitCorrelation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicCommitCorrelation {
    generation: u64,
    slot: ScanoutSlotId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingAtomicCommit {
    correlation: AtomicCommitCorrelation,
    allow_modeset: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtomicPageFlip {
    pub(crate) crtc_id: u32,
    pub(crate) tag: Option<AtomicPageFlipTag>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtomicPageFlipTag {
    Presentation(AtomicCommitCorrelation),
    Disable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicCommitError {
    operation: &'static str,
    errno: Option<i32>,
    detail: String,
}

impl AtomicCommitError {
    fn from_io(operation: &'static str, error: io::Error) -> Self {
        Self {
            operation,
            errno: error.raw_os_error(),
            detail: error.to_string(),
        }
    }

    fn synthetic(operation: &'static str, detail: impl Into<String>) -> Self {
        Self {
            operation,
            errno: None,
            detail: detail.into(),
        }
    }

    fn is_busy(&self) -> bool {
        self.errno == Some(libc::EBUSY)
    }

    pub(crate) fn authority_was_revoked(&self) -> bool {
        matches!(
            self.errno,
            Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::ENODEV)
        )
    }
}

impl fmt::Display for AtomicCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errno {
            Some(errno) => write!(
                formatter,
                "{} failed with errno {errno}: {}",
                self.operation, self.detail
            ),
            None => write!(formatter, "{} failed: {}", self.operation, self.detail),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicPresenterSetupError {
    pub(crate) code: &'static str,
    pub(crate) detail: String,
    commit: Option<AtomicCommitError>,
    pub(crate) retain_pool: bool,
}

impl AtomicPresenterSetupError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            commit: None,
            retain_pool: false,
        }
    }

    pub(crate) fn external(code: &'static str, detail: impl Into<String>) -> Self {
        Self::new(code, detail)
    }

    fn from_commit(code: &'static str, commit: AtomicCommitError) -> Self {
        Self {
            code,
            detail: commit.to_string(),
            commit: Some(commit),
            retain_pool: false,
        }
    }

    fn retained(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            commit: None,
            retain_pool: true,
        }
    }
}

impl fmt::Display for AtomicPresenterSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtomicWaitReady {
    Ready { drm: bool, cancel: bool },
    Deadline,
}

/// Injectable syscall seam for addfb/rmfb, commit, wait and event decode.
pub(crate) trait AtomicIo: Send + 'static {
    fn add_framebuffer(
        &mut self,
        slot: ScanoutSlotId,
        buffer: &dyn PlanarBuffer,
    ) -> Result<u32, String>;
    fn remove_framebuffer(&mut self, framebuffer: u32) -> Result<(), String>;
    fn commit(
        &mut self,
        request: &AtomicRequest,
        options: AtomicCommitOptions,
    ) -> Result<(), AtomicCommitError>;
    fn wait_ready(
        &mut self,
        crtc_id: u32,
        cancel: BorrowedFd<'_>,
        absolute_deadline: Instant,
    ) -> Result<AtomicWaitReady, String>;
    fn decode_pageflips(&mut self, crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String>;
}

/// Generation-aware cancellation publication plus the non-blocking eventfd
/// used to wake a presenter blocked in ppoll.
pub(crate) struct AtomicCancellation {
    event: OwnedFd,
    all_generations: AtomicBool,
    generation: AtomicU64,
}

impl AtomicCancellation {
    pub(crate) fn new() -> io::Result<Arc<Self>> {
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            event: unsafe { OwnedFd::from_raw_fd(raw) },
            all_generations: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }))
    }

    pub(crate) fn handle(self: &Arc<Self>) -> PresentationCancelHandle {
        let cancellation = Arc::clone(self);
        PresentationCancelHandle::from_callback(move |scope| cancellation.cancel(scope))
    }

    /// Retire only a stale generation publication after replacement authority
    /// is installed. An all-generations Stop is sticky for this pump lifetime,
    /// and a cancellation already naming this generation remains authoritative.
    pub(crate) fn arm_generation(&self, generation: u64) {
        if self.all_generations.load(Ordering::Acquire) {
            self.wake();
            return;
        }
        let published = self.generation.load(Ordering::Acquire);
        if published != 0 && published < generation {
            let _ =
                self.generation
                    .compare_exchange(published, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        self.drain_eventfd();
        if self.all_generations.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) == generation
        {
            // A cancel which raced the drain must remain waitable as well as
            // atomically visible; otherwise the presenter could sleep with a
            // true cancellation predicate and an empty eventfd.
            self.wake();
        }
    }

    fn cancel(&self, scope: CancelScope) {
        // This authority-loss path must remain lock-free and bounded even if a
        // kernel ioctl stalls. The presenter therefore re-arbitrates every
        // commit outcome against these atomics; suppressing a racing commit is
        // subordinate to waking pause/stop immediately and settling any
        // committed slot through the bounded held/drain path.
        match scope {
            CancelScope::Generation(generation) => {
                let mut published = self.generation.load(Ordering::Acquire);
                while published < generation {
                    match self.generation.compare_exchange_weak(
                        published,
                        generation,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => break,
                        Err(observed) => published = observed,
                    }
                }
            }
            CancelScope::AllGenerations => self.all_generations.store(true, Ordering::Release),
        }
        self.wake();
    }

    fn wake(&self) {
        let one = 1_u64.to_ne_bytes();
        let written =
            unsafe { libc::write(self.event.as_raw_fd(), one.as_ptr().cast(), one.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            // Saturation means the non-blocking eventfd is already readable.
            debug_assert_eq!(error.raw_os_error(), Some(libc::EAGAIN));
        }
    }

    fn cancelled(&self, generation: u64) -> bool {
        self.all_generations.load(Ordering::Acquire)
            || self.generation.load(Ordering::Acquire) == generation
    }

    /// Consume a readable eventfd publication which does not apply to the
    /// generation being presented. The atomic predicate remains authoritative;
    /// a racing applicable cancellation is re-published after the drain.
    fn drain_stale_publication(&self, generation: u64) {
        if self.cancelled(generation) {
            return;
        }
        self.drain_eventfd();
        if self.cancelled(generation) {
            self.wake();
        }
    }

    fn drain_eventfd(&self) {
        let mut value = 0_u64;
        loop {
            let read = unsafe {
                libc::read(
                    self.event.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read >= 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            debug_assert_eq!(error.raw_os_error(), Some(libc::EAGAIN));
            break;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitWaitOutcome {
    Committed,
    CommittedThenCancelled,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingFlipDrainOutcome {
    Drained,
    Cancelled,
    Deadline,
}

pub(crate) struct AtomicPresenter<I: AtomicIo> {
    io: I,
    selection: AtomicOutputSelection,
    properties: AtomicPropertyIds,
    mode_blob: u64,
    framebuffers: BTreeMap<ScanoutSlotId, u32>,
    cancellation: Arc<AtomicCancellation>,
    modeset_required: bool,
    pending_commit: Option<PendingAtomicCommit>,
}

impl<I: AtomicIo> AtomicPresenter<I> {
    pub(crate) fn from_parts(
        io: I,
        selection: AtomicOutputSelection,
        properties: AtomicPropertyIds,
        mode_blob: u64,
        framebuffers: BTreeMap<ScanoutSlotId, u32>,
        cancellation: Arc<AtomicCancellation>,
    ) -> Self {
        Self {
            io,
            selection,
            properties,
            mode_blob,
            framebuffers,
            cancellation,
            modeset_required: true,
            pending_commit: None,
        }
    }

    pub(crate) fn admission_probe(
        &mut self,
        slot: ScanoutSlotId,
        generation: u64,
        deadline: Instant,
    ) -> Result<(), AtomicPresenterSetupError> {
        let request = self.scanout_request(slot).map_err(|detail| {
            AtomicPresenterSetupError::new("kms-live-atomic-admission-request-invalid", detail)
        })?;
        for options in [
            AtomicCommitOptions {
                test_only: true,
                allow_modeset: true,
                nonblock: false,
                page_flip_event: false,
                correlation: None,
            },
            AtomicCommitOptions {
                test_only: true,
                allow_modeset: true,
                nonblock: true,
                page_flip_event: false,
                correlation: None,
            },
        ] {
            match self.commit_with_busy_retry(&request, options, generation, deadline) {
                Ok(CommitWaitOutcome::Committed) => {}
                Ok(CommitWaitOutcome::CommittedThenCancelled | CommitWaitOutcome::Cancelled) => {
                    return Err(AtomicPresenterSetupError::new(
                        "kms-live-atomic-admission-cancelled",
                        "cancellation preceded TEST_ONLY admission",
                    ));
                }
                Err(error) if error.is_busy() => {
                    return Err(AtomicPresenterSetupError::from_commit(
                        "kms-live-atomic-admission-busy-deadline",
                        error,
                    ));
                }
                Err(error) => {
                    return Err(AtomicPresenterSetupError::from_commit(
                        "kms-live-atomic-admission-hard-rejection",
                        error,
                    ));
                }
            }
        }
        // TEST_ONLY validates the property set but does not prove that the
        // driver's asynchronous execution path accepts NONBLOCK. The first
        // live NONBLOCK|PAGE_FLIP_EVENT modeset below is the real acceptance
        // gate and has its own named refusal.
        Ok(())
    }

    pub(crate) fn present(
        &mut self,
        slot: ScanoutSlotId,
        generation: u64,
        deadline: PresentDeadline,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
        let absolute_deadline = deadline.instant().ok_or_else(|| {
            KmsRenderPlatformFailure::terminal(
                "kms-live-atomic-present-unbounded",
                "atomic presentation requires an explicit absolute deadline",
            )
        })?;
        let request = self.scanout_request(slot).map_err(atomic_failure)?;
        self.present_request(
            slot,
            generation,
            absolute_deadline,
            request,
            self.modeset_required,
            false,
        )
    }

    /// Whether the next ordinary present will carry `ALLOW_MODESET`.
    /// The render coordinator uses the same state to choose the matching
    /// page-flip-event deadline before calling [`Self::present`].
    pub(crate) fn next_present_allows_modeset(&self) -> bool {
        self.modeset_required
    }

    /// Try the retained buffer as a same-mode plane flip. TEST_ONLY and the
    /// real request share the exact property builder and never carry
    /// ALLOW_MODESET. The caller owns fallback policy and may safely demote any
    /// returned failure while the overall resume deadline remains live.
    pub(crate) fn present_retained_seamless(
        &mut self,
        slot: ScanoutSlotId,
        generation: u64,
        deadline: PresentDeadline,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
        let absolute_deadline = deadline.instant().ok_or_else(|| {
            KmsRenderPlatformFailure::terminal(
                "kms-live-atomic-seamless-unbounded",
                "seamless resume requires an explicit absolute deadline",
            )
        })?;
        let request = self.pageflip_request(slot).map_err(atomic_failure)?;
        let test_options = AtomicCommitOptions {
            test_only: true,
            allow_modeset: false,
            nonblock: true,
            page_flip_event: false,
            correlation: None,
        };
        match self.commit_with_busy_retry(&request, test_options, generation, absolute_deadline) {
            Ok(CommitWaitOutcome::Committed) => {}
            Ok(CommitWaitOutcome::CommittedThenCancelled | CommitWaitOutcome::Cancelled) => {
                return Ok(PresentOutcome::Cancelled);
            }
            Err(error) => {
                return Err(KmsRenderPlatformFailure::terminal(
                    if error.is_busy() {
                        "kms-live-atomic-seamless-test-busy-deadline"
                    } else {
                        "kms-live-atomic-seamless-test-refused"
                    },
                    error.to_string(),
                ));
            }
        }
        self.present_request(slot, generation, absolute_deadline, request, false, true)
    }

    fn present_request(
        &mut self,
        slot: ScanoutSlotId,
        generation: u64,
        absolute_deadline: Instant,
        request: AtomicRequest,
        allow_modeset: bool,
        seamless: bool,
    ) -> Result<PresentOutcome, KmsRenderPlatformFailure> {
        // Cancellation is authoritative before the ioctl as well as after it:
        // a published authority loss must never enqueue a new scanout commit.
        if self.cancellation.cancelled(generation) {
            return Ok(PresentOutcome::Cancelled);
        }
        self.cancellation.drain_stale_publication(generation);
        if self.cancellation.cancelled(generation) {
            return Ok(PresentOutcome::Cancelled);
        }
        let correlation = AtomicCommitCorrelation { generation, slot };
        let options = AtomicCommitOptions {
            test_only: false,
            allow_modeset,
            nonblock: true,
            page_flip_event: true,
            correlation: Some(correlation),
        };
        match self.commit_with_busy_retry(&request, options, generation, absolute_deadline) {
            Ok(CommitWaitOutcome::Cancelled) => return Ok(PresentOutcome::Cancelled),
            Ok(CommitWaitOutcome::Committed) => {
                self.pending_commit = Some(PendingAtomicCommit {
                    correlation,
                    allow_modeset,
                });
            }
            Ok(CommitWaitOutcome::CommittedThenCancelled) => {
                self.pending_commit = Some(PendingAtomicCommit {
                    correlation,
                    allow_modeset,
                });
                return Ok(PresentOutcome::Cancelled);
            }
            Err(error) => {
                let code = if seamless && error.is_busy() {
                    "kms-live-atomic-seamless-flip-busy-deadline"
                } else if seamless {
                    "kms-live-atomic-seamless-flip-refused"
                } else if self.modeset_required && error.is_busy() {
                    "kms-live-atomic-first-nonblocking-modeset-busy-deadline"
                } else if self.modeset_required {
                    "kms-live-atomic-first-nonblocking-modeset-refused"
                } else if error.is_busy() {
                    "kms-live-atomic-commit-busy-deadline"
                } else {
                    "kms-live-atomic-commit-hard-rejection"
                };
                return Err(KmsRenderPlatformFailure::terminal(code, error.to_string()));
            }
        }

        loop {
            match self
                .io
                .wait_ready(
                    self.selection.crtc_id,
                    self.cancellation.event.as_fd(),
                    absolute_deadline,
                )
                .map_err(atomic_failure)?
            {
                AtomicWaitReady::Deadline => {
                    return Err(KmsRenderPlatformFailure::terminal(
                        ATOMIC_PRESENT_TIMEOUT_CODE,
                        format!(
                            "atomic pageflip for CRTC {} generation {generation} exceeded its absolute deadline",
                            self.selection.crtc_id
                        ),
                    ));
                }
                AtomicWaitReady::Ready { drm, cancel } => {
                    // Re-read the predicate after every wake, not merely when
                    // this ppoll snapshot reported the eventfd. A cancellation
                    // published after the kernel formed a DRM-only snapshot
                    // still wins before event decode or completion.
                    if self.cancellation.cancelled(generation) {
                        return Ok(PresentOutcome::Cancelled);
                    }
                    if cancel {
                        self.cancellation.drain_stale_publication(generation);
                        if self.cancellation.cancelled(generation) {
                            return Ok(PresentOutcome::Cancelled);
                        }
                    }
                    if drm {
                        let matching = self
                            .io
                            .decode_pageflips(self.selection.crtc_id)
                            .map_err(atomic_failure)?
                            .into_iter()
                            .any(|event| {
                                event.crtc_id == self.selection.crtc_id
                                    && event.tag
                                        == Some(AtomicPageFlipTag::Presentation(correlation))
                            });
                        // The matching kernel event has been consumed even if
                        // authority loss raced its decode. Retire pending state
                        // before final cancellation arbitration so teardown
                        // cannot wait for a phantom event that no longer exists.
                        if matching {
                            self.pending_commit = None;
                        }
                        // Decode may itself overlap the authority-loss callback.
                        // Take one final Acquire observation before declaring
                        // the kernel event displayed.
                        if self.cancellation.cancelled(generation) {
                            return Ok(PresentOutcome::Cancelled);
                        }
                        if matching {
                            // A retained same-mode flip deliberately does not
                            // establish the new generation's full property
                            // set. Keep the first fresh frame modeset-shaped so
                            // its new MODE_ID blob is paired with ALLOW_MODESET;
                            // some drivers treat a content-equivalent new blob
                            // id as a mode change.
                            if !seamless {
                                self.modeset_required = false;
                            }
                            return Ok(PresentOutcome::Displayed);
                        }
                    }
                    // EINTR, stale and unrelated events all retain this exact
                    // absolute deadline; no wake can renew the budget.
                }
            }
        }
    }

    fn commit_with_busy_retry(
        &mut self,
        request: &AtomicRequest,
        options: AtomicCommitOptions,
        generation: u64,
        absolute_deadline: Instant,
    ) -> Result<CommitWaitOutcome, AtomicCommitError> {
        loop {
            if self.cancellation.cancelled(generation) {
                return Ok(CommitWaitOutcome::Cancelled);
            }
            let commit = self.io.commit(request, options);
            match commit {
                Ok(()) if self.cancellation.cancelled(generation) => {
                    return Ok(CommitWaitOutcome::CommittedThenCancelled);
                }
                Ok(()) => return Ok(CommitWaitOutcome::Committed),
                Err(error) if error.is_busy() => {
                    if self.cancellation.cancelled(generation) {
                        tracing::debug!(
                            operation = error.operation,
                            ?error.errno,
                            detail = %error.detail,
                            "atomic EBUSY commit outcome was superseded by cancellation"
                        );
                        return Ok(CommitWaitOutcome::Cancelled);
                    }
                    match self
                        .io
                        .wait_ready(
                            self.selection.crtc_id,
                            self.cancellation.event.as_fd(),
                            absolute_deadline,
                        )
                        .map_err(|detail| {
                            AtomicCommitError::synthetic("atomic EBUSY wait", detail)
                        })? {
                        AtomicWaitReady::Deadline if self.cancellation.cancelled(generation) => {
                            return Ok(CommitWaitOutcome::Cancelled);
                        }
                        AtomicWaitReady::Deadline => return Err(error),
                        AtomicWaitReady::Ready { drm, cancel } => {
                            if self.cancellation.cancelled(generation) {
                                return Ok(CommitWaitOutcome::Cancelled);
                            }
                            if cancel {
                                self.cancellation.drain_stale_publication(generation);
                            }
                            if drm {
                                let _ = self.io.decode_pageflips(self.selection.crtc_id).map_err(
                                    |detail| {
                                        AtomicCommitError::synthetic(
                                            "atomic EBUSY event decode",
                                            detail,
                                        )
                                    },
                                )?;
                            }
                        }
                    }
                }
                Err(error) if self.cancellation.cancelled(generation) => {
                    tracing::debug!(
                        operation = error.operation,
                        ?error.errno,
                        detail = %error.detail,
                        "atomic hard commit failure was superseded by cancellation"
                    );
                    return Ok(CommitWaitOutcome::Cancelled);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn framebuffer(&self, slot: ScanoutSlotId) -> Option<u32> {
        self.framebuffers.get(&slot).copied()
    }

    pub(crate) fn has_pending_commit(&self) -> bool {
        self.pending_commit.is_some()
    }

    pub(crate) fn pending_commit_allows_modeset(&self) -> bool {
        self.pending_commit
            .is_some_and(|pending| pending.allow_modeset)
    }

    /// Select teardown's bounded drain budget from the commit that is still
    /// pending, never from the shape of a future presentation request.
    pub(crate) fn pending_commit_teardown_timeout(
        &self,
        steady_flip_timeout: Duration,
        modeset_timeout: Duration,
    ) -> Duration {
        if self.pending_commit_allows_modeset() {
            modeset_timeout
        } else {
            steady_flip_timeout
        }
    }

    /// Drain a pending seamless-resume flip while retaining the same
    /// cancellation and absolute-deadline contract as normal presentation.
    pub(crate) fn drain_pending_flip(
        &mut self,
        generation: u64,
        deadline: Instant,
    ) -> Result<PendingFlipDrainOutcome, String> {
        let Some(pending) = self.pending_commit else {
            return Ok(PendingFlipDrainOutcome::Drained);
        };
        if self.cancellation.cancelled(generation) {
            return Ok(PendingFlipDrainOutcome::Cancelled);
        }
        self.cancellation.drain_stale_publication(generation);
        if self.cancellation.cancelled(generation) {
            return Ok(PendingFlipDrainOutcome::Cancelled);
        }
        loop {
            match self.io.wait_ready(
                self.selection.crtc_id,
                self.cancellation.event.as_fd(),
                deadline,
            )? {
                AtomicWaitReady::Deadline => {
                    return Ok(if self.cancellation.cancelled(generation) {
                        PendingFlipDrainOutcome::Cancelled
                    } else {
                        PendingFlipDrainOutcome::Deadline
                    });
                }
                AtomicWaitReady::Ready { drm, cancel } => {
                    if self.cancellation.cancelled(generation) {
                        return Ok(PendingFlipDrainOutcome::Cancelled);
                    }
                    if cancel {
                        self.cancellation.drain_stale_publication(generation);
                        if self.cancellation.cancelled(generation) {
                            return Ok(PendingFlipDrainOutcome::Cancelled);
                        }
                    }
                    if drm {
                        let matching = self
                            .io
                            .decode_pageflips(self.selection.crtc_id)?
                            .into_iter()
                            .any(|event| {
                                event.crtc_id == self.selection.crtc_id
                                    && event.tag
                                        == Some(AtomicPageFlipTag::Presentation(
                                            pending.correlation,
                                        ))
                            });
                        if matching {
                            self.pending_commit = None;
                        }
                        if self.cancellation.cancelled(generation) {
                            return Ok(PendingFlipDrainOutcome::Cancelled);
                        }
                        if matching {
                            return Ok(PendingFlipDrainOutcome::Drained);
                        }
                    }
                }
            }
        }
    }

    /// Teardown must drain a commit which may itself have reported Cancelled.
    /// Its caller supplies the hard absolute teardown deadline; a quiet eventfd
    /// prevents the already-latched presentation cancellation from hot-looping.
    pub(crate) fn drain_pending_flip_for_teardown(
        &mut self,
        deadline: Instant,
    ) -> Result<bool, String> {
        let Some(pending) = self.pending_commit else {
            return Ok(true);
        };
        let quiet = AtomicCancellation::new()
            .map_err(|error| format!("atomic pending-flip eventfd creation failed: {error}"))?;
        loop {
            match self
                .io
                .wait_ready(self.selection.crtc_id, quiet.event.as_fd(), deadline)?
            {
                AtomicWaitReady::Deadline => return Ok(false),
                AtomicWaitReady::Ready { drm: true, .. } => {
                    if self
                        .io
                        .decode_pageflips(self.selection.crtc_id)?
                        .into_iter()
                        .any(|event| {
                            event.crtc_id == self.selection.crtc_id
                                && event.tag
                                    == Some(AtomicPageFlipTag::Presentation(pending.correlation))
                        })
                    {
                        self.pending_commit = None;
                        return Ok(true);
                    }
                }
                AtomicWaitReady::Ready { .. } => {}
            }
        }
    }

    pub(crate) fn disable_nonblocking(
        &mut self,
        deadline: Instant,
    ) -> Result<(), AtomicCommitError> {
        let quiet = AtomicCancellation::new()
            .map_err(|error| AtomicCommitError::from_io("atomic disable eventfd", error))?;
        let request = build_disable_request(self.selection, self.properties);
        let options = AtomicCommitOptions {
            test_only: false,
            allow_modeset: true,
            nonblock: true,
            page_flip_event: true,
            correlation: None,
        };
        loop {
            match self.io.commit(&request, options) {
                Ok(()) => break,
                Err(error) if error.is_busy() => {
                    match self
                        .io
                        .wait_ready(self.selection.crtc_id, quiet.event.as_fd(), deadline)
                        .map_err(|detail| {
                            AtomicCommitError::synthetic("atomic disable EBUSY wait", detail)
                        })? {
                        AtomicWaitReady::Deadline => {
                            return Err(AtomicCommitError::synthetic(
                                "atomic disable EBUSY deadline",
                                error.to_string(),
                            ));
                        }
                        AtomicWaitReady::Ready { drm: true, .. } => {
                            let events = self.io.decode_pageflips(self.selection.crtc_id).map_err(
                                |detail| {
                                    AtomicCommitError::synthetic(
                                        "atomic disable EBUSY event decode",
                                        detail,
                                    )
                                },
                            )?;
                            if let Some(pending) = self.pending_commit
                                && events.into_iter().any(|event| {
                                    event.crtc_id == self.selection.crtc_id
                                        && event.tag
                                            == Some(AtomicPageFlipTag::Presentation(
                                                pending.correlation,
                                            ))
                                })
                            {
                                self.pending_commit = None;
                            }
                        }
                        AtomicWaitReady::Ready { .. } => {}
                    }
                }
                Err(error) => return Err(error),
            }
        }
        loop {
            match self
                .io
                .wait_ready(self.selection.crtc_id, quiet.event.as_fd(), deadline)
                .map_err(|detail| AtomicCommitError::synthetic("atomic disable wait", detail))?
            {
                AtomicWaitReady::Deadline => {
                    return Err(AtomicCommitError::synthetic(
                        "atomic non-blocking disable deadline",
                        "pageflip exceeded its original absolute deadline",
                    ));
                }
                AtomicWaitReady::Ready { drm: true, .. }
                    if self
                        .io
                        .decode_pageflips(self.selection.crtc_id)
                        .map_err(|detail| {
                            AtomicCommitError::synthetic("atomic disable event decode", detail)
                        })?
                        .into_iter()
                        .any(|event| {
                            event.crtc_id == self.selection.crtc_id
                                && event.tag == Some(AtomicPageFlipTag::Disable)
                        }) =>
                {
                    return Ok(());
                }
                AtomicWaitReady::Ready { .. } => {}
            }
        }
    }

    pub(crate) fn remove_framebuffers(
        &mut self,
        slot_states: &[(ScanoutSlotId, ScanoutSlotState)],
        authority_revoked: bool,
    ) -> Result<(), String> {
        for (slot, state) in slot_states {
            if matches!(state, ScanoutSlotState::Queued | ScanoutSlotState::Front)
                && !authority_revoked
            {
                #[cfg(not(test))]
                debug_assert!(
                    false,
                    "RmFB of live scanout slot {} in {state:?} without revoked authority",
                    slot.0
                );
                return Err(format!(
                    "kms-live-atomic-rmfb-live-slot-refused: slot {} remains {state:?} without revoked authority",
                    slot.0
                ));
            }
        }
        let mut first = None;
        for (slot, framebuffer) in self
            .framebuffers
            .iter()
            .map(|(slot, framebuffer)| (*slot, *framebuffer))
            .collect::<Vec<_>>()
        {
            match self.io.remove_framebuffer(framebuffer) {
                Ok(()) => {
                    self.framebuffers.remove(&slot);
                }
                Err(error) if first.is_none() => first = Some(error),
                Err(_) => {}
            }
        }
        first.map_or(Ok(()), Err)
    }

    fn scanout_request(&self, slot: ScanoutSlotId) -> Result<AtomicRequest, String> {
        let framebuffer = self
            .framebuffers
            .get(&slot)
            .copied()
            .ok_or_else(|| format!("atomic scanout slot {} has no framebuffer", slot.0))?;
        Ok(build_scanout_request(
            self.selection,
            self.properties,
            self.mode_blob,
            framebuffer,
        ))
    }

    fn pageflip_request(&self, slot: ScanoutSlotId) -> Result<AtomicRequest, String> {
        let framebuffer = self
            .framebuffers
            .get(&slot)
            .copied()
            .ok_or_else(|| format!("atomic scanout slot {} has no framebuffer", slot.0))?;
        Ok(build_pageflip_request(
            self.selection,
            self.properties,
            framebuffer,
        ))
    }
}

impl AtomicPresenter<ProductionAtomicIo> {
    pub(crate) fn production(
        fd: OwnedFd,
        pool: &ScanoutPool,
        cancellation: Arc<AtomicCancellation>,
        events: Arc<ProductionAtomicEventRouter>,
        generation: u64,
        admission_deadline: Instant,
    ) -> Result<Self, AtomicPresenterSetupError> {
        let selection = pool.selection();
        let mut io = ProductionAtomicIo::new(fd, events);
        let properties = io.property_ids(selection).map_err(|detail| {
            AtomicPresenterSetupError::new("kms-live-atomic-property-map-failed", detail)
        })?;
        let mode_blob = io.create_mode_blob(selection).map_err(|detail| {
            AtomicPresenterSetupError::new("kms-live-atomic-mode-blob-failed", detail)
        })?;
        let mut framebuffers = BTreeMap::new();
        for slot in pool.slot_ids().collect::<Vec<_>>() {
            let buffer = pool.gbm_buffer(slot).map_err(|error| {
                AtomicPresenterSetupError::new(
                    "kms-live-atomic-scanout-buffer-missing",
                    error.to_string(),
                )
            })?;
            let framebuffer = match io.add_framebuffer(slot, buffer) {
                Ok(framebuffer) => framebuffer,
                Err(error) => {
                    let mut cleanup_failure = None;
                    for framebuffer in framebuffers.values().copied() {
                        if let Err(cleanup) = io.remove_framebuffer(framebuffer)
                            && cleanup_failure.is_none()
                        {
                            cleanup_failure = Some(cleanup);
                        }
                    }
                    if let Err(cleanup) = io.destroy_mode_blob(mode_blob)
                        && cleanup_failure.is_none()
                    {
                        cleanup_failure = Some(cleanup);
                    }
                    if let Some(cleanup) = cleanup_failure {
                        std::mem::forget(io);
                        return Err(AtomicPresenterSetupError::retained(
                            "kms-live-atomic-admission-ownership-retained",
                            format!(
                                "AddFB failed ({error}); cleanup was unproved ({cleanup}); atomic I/O ownership was deliberately retained"
                            ),
                        ));
                    }
                    return Err(AtomicPresenterSetupError::new(
                        "kms-live-atomic-addfb-failed",
                        error,
                    ));
                }
            };
            framebuffers.insert(slot, framebuffer);
        }
        let mut presenter = Self::from_parts(
            io,
            selection,
            properties,
            mode_blob,
            framebuffers,
            cancellation,
        );
        let probe_slot = pool.slot_ids().next().ok_or_else(|| {
            AtomicPresenterSetupError::new(
                "kms-live-atomic-scanout-pool-empty",
                "atomic scanout pool has no framebuffer slots",
            )
        })?;
        if let Err(error) = presenter.admission_probe(probe_slot, generation, admission_deadline) {
            let framebuffer_cleanup = presenter.remove_framebuffers(&pool.slot_state_view(), false);
            let blob_cleanup = presenter.destroy_mode_blob();
            if framebuffer_cleanup.is_err() || blob_cleanup.is_err() {
                let cleanup = framebuffer_cleanup
                    .err()
                    .or_else(|| blob_cleanup.err())
                    .expect("one cleanup leg failed");
                let admission = error.to_string();
                std::mem::forget(presenter);
                return Err(AtomicPresenterSetupError::retained(
                    "kms-live-atomic-admission-ownership-retained",
                    format!(
                        "admission failed ({admission}); cleanup was unproved ({cleanup}); atomic presenter ownership was deliberately retained"
                    ),
                ));
            }
            return Err(error);
        }
        Ok(presenter)
    }

    pub(crate) fn destroy_mode_blob(&self) -> Result<(), String> {
        self.io.destroy_mode_blob(self.mode_blob)
    }
}

fn atomic_failure(detail: impl Into<String>) -> KmsRenderPlatformFailure {
    KmsRenderPlatformFailure::terminal("kms-live-atomic-presentation-failed", detail.into())
}

fn build_scanout_request(
    selection: AtomicOutputSelection,
    ids: AtomicPropertyIds,
    mode_blob: u64,
    framebuffer: u32,
) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    request.set(
        selection.connector_id,
        ids.connector_crtc_id,
        u64::from(selection.crtc_id),
    );
    request.set(selection.crtc_id, ids.crtc_mode_id, mode_blob);
    request.set(selection.crtc_id, ids.crtc_active, 1);
    request.set(
        selection.primary_plane_id,
        ids.plane_fb_id,
        u64::from(framebuffer),
    );
    request.set(
        selection.primary_plane_id,
        ids.plane_crtc_id,
        u64::from(selection.crtc_id),
    );
    request.set(selection.primary_plane_id, ids.plane_src_x, 0);
    request.set(selection.primary_plane_id, ids.plane_src_y, 0);
    request.set(
        selection.primary_plane_id,
        ids.plane_src_w,
        u64::from(selection.mode.width) << 16,
    );
    request.set(
        selection.primary_plane_id,
        ids.plane_src_h,
        u64::from(selection.mode.height) << 16,
    );
    request.set(selection.primary_plane_id, ids.plane_crtc_x, 0);
    request.set(selection.primary_plane_id, ids.plane_crtc_y, 0);
    request.set(
        selection.primary_plane_id,
        ids.plane_crtc_w,
        u64::from(selection.mode.width),
    );
    request.set(
        selection.primary_plane_id,
        ids.plane_crtc_h,
        u64::from(selection.mode.height),
    );
    request
}

fn build_disable_request(
    selection: AtomicOutputSelection,
    ids: AtomicPropertyIds,
) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    request.set(selection.connector_id, ids.connector_crtc_id, 0);
    request.set(selection.crtc_id, ids.crtc_mode_id, 0);
    request.set(selection.crtc_id, ids.crtc_active, 0);
    request.set(selection.primary_plane_id, ids.plane_fb_id, 0);
    request.set(selection.primary_plane_id, ids.plane_crtc_id, 0);
    request
}

fn build_pageflip_request(
    selection: AtomicOutputSelection,
    ids: AtomicPropertyIds,
    framebuffer: u32,
) -> AtomicRequest {
    let mut request = AtomicRequest::default();
    request.set(
        selection.primary_plane_id,
        ids.plane_fb_id,
        u64::from(framebuffer),
    );
    request
}

pub(crate) struct ProductionAtomicIo {
    card: AtomicCard,
    events: Arc<ProductionAtomicEventRouter>,
}

/// The sole reader and kernel-user-data registry for one DRM file
/// description. Presenter commit descriptors may be duplicated, but every
/// page-flip read is routed through this one per-device object so outputs can
/// never cross-consume the shared event queue.
pub(crate) struct ProductionAtomicEventRouter {
    fd: OwnedFd,
    state: Mutex<AtomicEventRouterState>,
}

struct AtomicEventRouterState {
    next_token: u64,
    pending: BTreeMap<u64, AtomicPageFlipTag>,
    completed: BTreeMap<u32, VecDeque<AtomicPageFlip>>,
}

impl ProductionAtomicEventRouter {
    pub(crate) fn new(fd: OwnedFd) -> Result<Arc<Self>, String> {
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(format!(
                "atomic event-router F_GETFL failed: {}",
                io::Error::last_os_error()
            ));
        }
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(format!(
                "atomic event-router could not enable non-blocking reads: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Arc::new(Self {
            fd,
            state: Mutex::new(AtomicEventRouterState {
                next_token: 1,
                pending: BTreeMap::new(),
                completed: BTreeMap::new(),
            }),
        }))
    }

    fn register(&self, tag: AtomicPageFlipTag) -> Result<u64, AtomicCommitError> {
        let mut state = self.state.lock().map_err(|_| {
            AtomicCommitError::synthetic(
                "atomic event registration",
                "event-router registry was poisoned",
            )
        })?;
        let token = state.next_token;
        state.next_token = state.next_token.checked_add(1).ok_or_else(|| {
            AtomicCommitError::synthetic(
                "atomic event registration",
                "kernel user_data token space was exhausted",
            )
        })?;
        state.pending.insert(token, tag);
        Ok(token)
    }

    fn unregister(&self, token: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(&token);
        }
    }

    fn wait_ready(
        &self,
        crtc_id: u32,
        cancel: BorrowedFd<'_>,
        absolute_deadline: Instant,
    ) -> Result<AtomicWaitReady, String> {
        if self
            .state
            .lock()
            .map_err(|_| "atomic event-router registry was poisoned".to_string())?
            .completed
            .get(&crtc_id)
            .is_some_and(|events| !events.is_empty())
        {
            return Ok(AtomicWaitReady::Ready {
                drm: true,
                cancel: false,
            });
        }
        wait_on_drm_and_cancel(self.fd.as_fd(), cancel, absolute_deadline)
    }

    fn decode_pageflips(&self, crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String> {
        let queued = take_routed_pageflips(&self.state, crtc_id)?;
        if !queued.is_empty() {
            return Ok(queued);
        }
        let mut buffer = [0_u8; 4096];
        let amount = loop {
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read >= 0 {
                break read as usize;
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() == Some(libc::EAGAIN) {
                break 0;
            }
            return Err(format!("atomic DRM event read failed: {error}"));
        };
        let decoded = decode_raw_pageflips(&buffer[..amount], &self.state)?;
        route_pageflips(&self.state, decoded, crtc_id)
    }
}

fn route_pageflips(
    state: &Mutex<AtomicEventRouterState>,
    decoded: impl IntoIterator<Item = AtomicPageFlip>,
    requested_crtc: u32,
) -> Result<Vec<AtomicPageFlip>, String> {
    let mut state = state
        .lock()
        .map_err(|_| "atomic event-router registry was poisoned".to_string())?;
    for event in decoded {
        state
            .completed
            .entry(event.crtc_id)
            .or_default()
            .push_back(event);
    }
    Ok(state
        .completed
        .remove(&requested_crtc)
        .unwrap_or_default()
        .into_iter()
        .collect())
}

fn take_routed_pageflips(
    state: &Mutex<AtomicEventRouterState>,
    crtc_id: u32,
) -> Result<Vec<AtomicPageFlip>, String> {
    Ok(state
        .lock()
        .map_err(|_| "atomic event-router registry was poisoned".to_string())?
        .completed
        .remove(&crtc_id)
        .unwrap_or_default()
        .into_iter()
        .collect())
}

struct AtomicCard(OwnedFd);

impl AsFd for AtomicCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDrmDevice for AtomicCard {}
impl ControlDevice for AtomicCard {}

impl ProductionAtomicIo {
    pub(crate) fn new(fd: OwnedFd, events: Arc<ProductionAtomicEventRouter>) -> Self {
        Self {
            card: AtomicCard(fd),
            events,
        }
    }

    pub(crate) fn property_ids(
        &self,
        selection: AtomicOutputSelection,
    ) -> Result<AtomicPropertyIds, String> {
        let connector = control::from_u32::<control::connector::Handle>(selection.connector_id)
            .ok_or_else(|| "atomic connector id is zero".to_string())?;
        let crtc = control::from_u32::<control::crtc::Handle>(selection.crtc_id)
            .ok_or_else(|| "atomic CRTC id is zero".to_string())?;
        let plane = control::from_u32::<control::plane::Handle>(selection.primary_plane_id)
            .ok_or_else(|| "atomic primary-plane id is zero".to_string())?;
        Ok(AtomicPropertyIds {
            connector_crtc_id: property_id(&self.card, connector, "CRTC_ID")?,
            crtc_active: property_id(&self.card, crtc, "ACTIVE")?,
            crtc_mode_id: property_id(&self.card, crtc, "MODE_ID")?,
            plane_fb_id: property_id(&self.card, plane, "FB_ID")?,
            plane_crtc_id: property_id(&self.card, plane, "CRTC_ID")?,
            plane_crtc_x: property_id(&self.card, plane, "CRTC_X")?,
            plane_crtc_y: property_id(&self.card, plane, "CRTC_Y")?,
            plane_crtc_w: property_id(&self.card, plane, "CRTC_W")?,
            plane_crtc_h: property_id(&self.card, plane, "CRTC_H")?,
            plane_src_x: property_id(&self.card, plane, "SRC_X")?,
            plane_src_y: property_id(&self.card, plane, "SRC_Y")?,
            plane_src_w: property_id(&self.card, plane, "SRC_W")?,
            plane_src_h: property_id(&self.card, plane, "SRC_H")?,
        })
    }

    pub(crate) fn create_mode_blob(&self, selection: AtomicOutputSelection) -> Result<u64, String> {
        let connector = control::from_u32::<control::connector::Handle>(selection.connector_id)
            .ok_or_else(|| "atomic connector id is zero".to_string())?;
        let info = self
            .card
            .get_connector(connector, false)
            .map_err(|error| format!("atomic connector mode lookup failed: {error}"))?;
        let mode = info
            .modes()
            .iter()
            .find(|mode| connector_mode(mode) == selection.mode)
            .ok_or_else(|| "atomic selected mode disappeared before blob creation".to_string())?;
        let blob: control::property::RawValue = self
            .card
            .create_property_blob(mode)
            .map_err(|error| format!("atomic mode-blob creation failed: {error}"))?
            .into();
        Ok(blob)
    }

    pub(crate) fn destroy_mode_blob(&self, blob: u64) -> Result<(), String> {
        self.card
            .destroy_property_blob(blob)
            .map_err(|error| format!("atomic mode-blob destruction failed: {error}"))
    }
}

fn property_id<H: ResourceHandle>(card: &AtomicCard, object: H, name: &str) -> Result<u32, String> {
    let properties = card
        .get_properties(object)
        .map_err(|error| format!("atomic {name} property enumeration failed: {error}"))?;
    for property in properties.as_props_and_values().0 {
        let info = card
            .get_property(*property)
            .map_err(|error| format!("atomic property metadata failed: {error}"))?;
        if info.name().to_bytes() == name.as_bytes() {
            return Ok((*property).into());
        }
    }
    let raw: NonZeroU32 = object.into();
    Err(format!(
        "atomic object {} is missing required {name} property",
        raw.get()
    ))
}

#[repr(C)]
struct DrmModeAtomic {
    flags: u32,
    count_objs: u32,
    objects: u64,
    property_counts: u64,
    properties: u64,
    values: u64,
    reserved: u64,
    user_data: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmEventHeader {
    kind: u32,
    length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmEventVblank {
    header: DrmEventHeader,
    user_data: u64,
    tv_sec: u32,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32,
}

const DRM_EVENT_FLIP_COMPLETE: u32 = 2;

const fn drm_ioctl_mode_atomic() -> libc::c_ulong {
    const IOC_WRITE: u64 = 1;
    const IOC_READ: u64 = 2;
    const IOC_TYPESHIFT: u64 = 8;
    const IOC_SIZESHIFT: u64 = 16;
    const IOC_DIRSHIFT: u64 = 30;
    (((IOC_READ | IOC_WRITE) << IOC_DIRSHIFT)
        | ((std::mem::size_of::<DrmModeAtomic>() as u64) << IOC_SIZESHIFT)
        | ((b'd' as u64) << IOC_TYPESHIFT)
        | 0xbc) as libc::c_ulong
}

fn wait_on_drm_and_cancel(
    drm: BorrowedFd<'_>,
    cancel: BorrowedFd<'_>,
    absolute_deadline: Instant,
) -> Result<AtomicWaitReady, String> {
    wait_with_absolute_deadline(absolute_deadline, Instant::now, |remaining| {
        let timeout = libc::timespec {
            tv_sec: remaining.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
            tv_nsec: remaining.subsec_nanos() as libc::c_long,
        };
        let mut descriptors = [
            libc::pollfd {
                fd: drm.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe {
            libc::ppoll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                &timeout,
                std::ptr::null(),
            )
        };
        if ready == 0 {
            return Ok(AtomicWaitReady::Deadline);
        }
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(AtomicWaitReady::Ready {
            drm: descriptors[0].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0,
            cancel: descriptors[1].revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0,
        })
    })
}

fn wait_with_absolute_deadline(
    absolute_deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut poll: impl FnMut(std::time::Duration) -> io::Result<AtomicWaitReady>,
) -> Result<AtomicWaitReady, String> {
    loop {
        let current = now();
        if current >= absolute_deadline {
            return Ok(AtomicWaitReady::Deadline);
        }
        let remaining = absolute_deadline.saturating_duration_since(current);
        match poll(remaining) {
            Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
            Err(error) => return Err(format!("atomic pageflip ppoll failed: {error}")),
            Ok(ready) => return Ok(ready),
        }
    }
}

fn decode_raw_pageflips(
    buffer: &[u8],
    state: &Mutex<AtomicEventRouterState>,
) -> Result<Vec<AtomicPageFlip>, String> {
    let mut offset = 0_usize;
    let mut flips = Vec::new();
    while offset < buffer.len() {
        if buffer.len() - offset < std::mem::size_of::<DrmEventHeader>() {
            return Err("atomic DRM event ended inside its header".into());
        }
        let header = unsafe {
            std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<DrmEventHeader>())
        };
        let length = header.length as usize;
        if length < std::mem::size_of::<DrmEventHeader>()
            || offset
                .checked_add(length)
                .is_none_or(|end| end > buffer.len())
        {
            return Err("atomic DRM event has an invalid length".into());
        }
        if header.kind == DRM_EVENT_FLIP_COMPLETE {
            if length < std::mem::size_of::<DrmEventVblank>() {
                return Err("atomic pageflip event is shorter than drm_event_vblank".into());
            }
            let event = unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<DrmEventVblank>())
            };
            let tag = state
                .lock()
                .map_err(|_| "atomic event-router registry was poisoned".to_string())?
                .pending
                .remove(&event.user_data);
            flips.push(AtomicPageFlip {
                crtc_id: event.crtc_id,
                tag,
            });
        }
        offset += length;
    }
    Ok(flips)
}

impl AtomicIo for ProductionAtomicIo {
    fn add_framebuffer(
        &mut self,
        _slot: ScanoutSlotId,
        buffer: &dyn PlanarBuffer,
    ) -> Result<u32, String> {
        self.card
            .add_planar_framebuffer(buffer, control::FbCmd2Flags::MODIFIERS)
            .map(u32::from)
            .map_err(|error| format!("atomic AddFB2 with modifier failed: {error}"))
    }

    fn remove_framebuffer(&mut self, framebuffer: u32) -> Result<(), String> {
        let handle = control::from_u32::<control::framebuffer::Handle>(framebuffer)
            .ok_or_else(|| "atomic framebuffer id is zero".to_string())?;
        self.card
            .destroy_framebuffer(handle)
            .map_err(|error| format!("atomic RmFB failed: {error}"))
    }

    fn commit(
        &mut self,
        request: &AtomicRequest,
        options: AtomicCommitOptions,
    ) -> Result<(), AtomicCommitError> {
        let mut objects = Vec::<u32>::new();
        let mut property_counts = Vec::<u32>::new();
        let mut properties = Vec::<u32>::new();
        let mut values = Vec::<u64>::new();
        for property in &request.properties {
            if property.object == 0 || property.property == 0 {
                return Err(AtomicCommitError::synthetic(
                    "atomic commit request construction",
                    "object and property ids must be non-zero",
                ));
            }
            match objects.iter().position(|object| *object == property.object) {
                Some(index) if index + 1 != objects.len() => {
                    return Err(AtomicCommitError::synthetic(
                        "atomic commit request construction",
                        "properties for one object must be contiguous",
                    ));
                }
                Some(index) => property_counts[index] = property_counts[index].saturating_add(1),
                None => {
                    objects.push(property.object);
                    property_counts.push(1);
                }
            }
            properties.push(property.property);
            values.push(property.value);
        }
        let mut flags = control::AtomicCommitFlags::empty();
        flags.set(control::AtomicCommitFlags::TEST_ONLY, options.test_only);
        flags.set(
            control::AtomicCommitFlags::ALLOW_MODESET,
            options.allow_modeset,
        );
        flags.set(control::AtomicCommitFlags::NONBLOCK, options.nonblock);
        flags.set(
            control::AtomicCommitFlags::PAGE_FLIP_EVENT,
            options.page_flip_event,
        );
        let token = if options.page_flip_event && !options.test_only {
            let tag = options
                .correlation
                .map(AtomicPageFlipTag::Presentation)
                .unwrap_or(AtomicPageFlipTag::Disable);
            self.events.register(tag)?
        } else {
            0
        };
        let mut raw = DrmModeAtomic {
            flags: flags.bits(),
            count_objs: objects.len().try_into().map_err(|_| {
                AtomicCommitError::synthetic(
                    "atomic commit request construction",
                    "object count exceeds u32",
                )
            })?,
            objects: objects.as_mut_ptr() as u64,
            property_counts: property_counts.as_mut_ptr() as u64,
            properties: properties.as_mut_ptr() as u64,
            values: values.as_mut_ptr() as u64,
            reserved: 0,
            user_data: token,
        };
        let result = unsafe {
            libc::ioctl(
                self.card.as_fd().as_raw_fd(),
                drm_ioctl_mode_atomic(),
                &mut raw,
            )
        };
        if result < 0 {
            if token != 0 {
                self.events.unregister(token);
            }
            return Err(AtomicCommitError::from_io(
                "atomic commit ioctl",
                io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn wait_ready(
        &mut self,
        crtc_id: u32,
        cancel: BorrowedFd<'_>,
        absolute_deadline: Instant,
    ) -> Result<AtomicWaitReady, String> {
        self.events.wait_ready(crtc_id, cancel, absolute_deadline)
    }

    fn decode_pageflips(&mut self, crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String> {
        self.events.decode_pageflips(crtc_id)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Barrier, atomic::AtomicUsize, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use drm_fourcc::DrmFourcc;

    use super::*;

    #[derive(Default)]
    struct FakeAtomicIo {
        waits: VecDeque<AtomicWaitReady>,
        events: VecDeque<Vec<AtomicPageFlip>>,
        observed_deadlines: Vec<Instant>,
        commits: Vec<AtomicCommitOptions>,
        requests: Vec<AtomicRequest>,
        commit_results: VecDeque<Result<(), AtomicCommitError>>,
        cancel_on_commit: Option<(Arc<AtomicCancellation>, CancelScope)>,
        cancel_on_wait: Option<(Arc<AtomicCancellation>, CancelScope)>,
        cancel_on_decode: Option<(Arc<AtomicCancellation>, CancelScope)>,
        remove_results: VecDeque<Result<(), String>>,
        removed_framebuffers: Vec<u32>,
    }

    impl AtomicIo for FakeAtomicIo {
        fn add_framebuffer(
            &mut self,
            _slot: ScanoutSlotId,
            _buffer: &dyn PlanarBuffer,
        ) -> Result<u32, String> {
            Ok(99)
        }

        fn remove_framebuffer(&mut self, framebuffer: u32) -> Result<(), String> {
            self.removed_framebuffers.push(framebuffer);
            self.remove_results.pop_front().unwrap_or(Ok(()))
        }

        fn commit(
            &mut self,
            request: &AtomicRequest,
            options: AtomicCommitOptions,
        ) -> Result<(), AtomicCommitError> {
            self.commits.push(options);
            self.requests.push(request.clone());
            if let Some((cancellation, scope)) = self.cancel_on_commit.take() {
                cancellation.cancel(scope);
            }
            self.commit_results.pop_front().unwrap_or(Ok(()))
        }

        fn wait_ready(
            &mut self,
            _crtc_id: u32,
            _cancel: BorrowedFd<'_>,
            absolute_deadline: Instant,
        ) -> Result<AtomicWaitReady, String> {
            self.observed_deadlines.push(absolute_deadline);
            if let Some((cancellation, scope)) = self.cancel_on_wait.take() {
                cancellation.cancel(scope);
            }
            Ok(self.waits.pop_front().unwrap_or(AtomicWaitReady::Deadline))
        }

        fn decode_pageflips(&mut self, _crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String> {
            if let Some((cancellation, scope)) = self.cancel_on_decode.take() {
                cancellation.cancel(scope);
            }
            Ok(self.events.pop_front().unwrap_or_default())
        }
    }

    struct StalledCommitIo {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
        commits: Arc<AtomicUsize>,
    }

    impl AtomicIo for StalledCommitIo {
        fn add_framebuffer(
            &mut self,
            _slot: ScanoutSlotId,
            _buffer: &dyn PlanarBuffer,
        ) -> Result<u32, String> {
            Ok(99)
        }

        fn remove_framebuffer(&mut self, _framebuffer: u32) -> Result<(), String> {
            Ok(())
        }

        fn commit(
            &mut self,
            _request: &AtomicRequest,
            _options: AtomicCommitOptions,
        ) -> Result<(), AtomicCommitError> {
            self.commits.fetch_add(1, Ordering::AcqRel);
            self.entered.wait();
            self.release.wait();
            Ok(())
        }

        fn wait_ready(
            &mut self,
            _crtc_id: u32,
            _cancel: BorrowedFd<'_>,
            _absolute_deadline: Instant,
        ) -> Result<AtomicWaitReady, String> {
            Ok(AtomicWaitReady::Deadline)
        }

        fn decode_pageflips(&mut self, _crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String> {
            Ok(Vec::new())
        }
    }

    fn selection() -> AtomicOutputSelection {
        AtomicOutputSelection {
            connector_id: 10,
            crtc_id: 20,
            primary_plane_id: 30,
            mode: super::super::kms::ConnectorMode {
                width: 3840,
                height: 2160,
                refresh_millihz: 60_000,
                preferred: true,
                clock_khz: 1,
                hsync: (1, 1, 1),
                vsync: (1, 1, 1),
                hskew: 0,
                vscan: 0,
                flags: 0,
            },
            format: DrmFourcc::Xrgb8888 as u32,
            modifier: 1,
        }
    }

    fn property_ids() -> AtomicPropertyIds {
        AtomicPropertyIds {
            connector_crtc_id: 1,
            crtc_active: 2,
            crtc_mode_id: 3,
            plane_fb_id: 4,
            plane_crtc_id: 5,
            plane_crtc_x: 6,
            plane_crtc_y: 7,
            plane_crtc_w: 8,
            plane_crtc_h: 9,
            plane_src_x: 10,
            plane_src_y: 11,
            plane_src_w: 12,
            plane_src_h: 13,
        }
    }

    fn presenter(io: FakeAtomicIo) -> AtomicPresenter<FakeAtomicIo> {
        presenter_with_cancellation(io, AtomicCancellation::new().expect("eventfd"))
    }

    fn presenter_with_cancellation(
        io: FakeAtomicIo,
        cancellation: Arc<AtomicCancellation>,
    ) -> AtomicPresenter<FakeAtomicIo> {
        AtomicPresenter::from_parts(
            io,
            selection(),
            property_ids(),
            40,
            BTreeMap::from([(ScanoutSlotId(0), 50)]),
            cancellation,
        )
    }

    #[test]
    fn atomic_present_completes_on_matching_pageflip() {
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        let mut presenter = presenter(io);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Displayed)
        );
    }

    fn matching_flip(generation: u64) -> AtomicPageFlip {
        AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation,
                slot: ScanoutSlotId(0),
            })),
        }
    }

    #[test]
    fn compatible_retained_state_drives_same_mode_pageflip() {
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![matching_flip(7)]);
        let mut presenter = presenter(io);

        assert_eq!(
            presenter.present_retained_seamless(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Displayed)
        );
        assert_eq!(presenter.io.commits.len(), 2);
        assert!(presenter.io.commits[0].test_only);
        assert!(!presenter.io.commits[0].allow_modeset);
        assert!(!presenter.io.commits[1].test_only);
        assert!(!presenter.io.commits[1].allow_modeset);
        assert_eq!(presenter.io.requests[0], presenter.io.requests[1]);
        assert_eq!(presenter.io.requests[0].properties.len(), 1);
        assert_eq!(
            presenter.io.requests[0].properties[0],
            AtomicProperty {
                object: 30,
                property: 4,
                value: 50,
            }
        );
        assert!(
            presenter.next_present_allows_modeset(),
            "the retained flip did not install the new MODE_ID property set"
        );

        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        presenter.io.events.push_back(vec![matching_flip(7)]);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Displayed)
        );
        assert!(
            presenter.io.commits[2].allow_modeset,
            "the first fresh frame must pair the new MODE_ID with ALLOW_MODESET"
        );
        assert!(!presenter.next_present_allows_modeset());
    }

    #[test]
    fn failed_seamless_test_leaves_full_modeset_fallback_available() {
        let mut io = FakeAtomicIo::default();
        io.commit_results
            .extend([Err(commit_errno(libc::EINVAL)), Ok(())]);
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![matching_flip(7)]);
        let mut presenter = presenter(io);
        let deadline = PresentDeadline::bounded(Instant::now() + Duration::from_secs(1));

        let failure = presenter
            .present_retained_seamless(ScanoutSlotId(0), 7, deadline)
            .expect_err("same-mode TEST_ONLY refusal is recoverable by the caller");
        assert_eq!(failure.code, "kms-live-atomic-seamless-test-refused");
        assert_eq!(
            presenter.present(ScanoutSlotId(0), 7, deadline),
            Ok(PresentOutcome::Displayed)
        );
        assert!(presenter.io.commits[0].test_only);
        assert!(!presenter.io.commits[0].allow_modeset);
        assert!(!presenter.io.commits[1].test_only);
        assert!(presenter.io.commits[1].allow_modeset);
    }

    #[test]
    fn failed_seamless_flip_leaves_full_modeset_fallback_available() {
        let mut io = FakeAtomicIo::default();
        io.commit_results
            .extend([Ok(()), Err(commit_errno(libc::EINVAL)), Ok(())]);
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![matching_flip(7)]);
        let mut presenter = presenter(io);
        let deadline = PresentDeadline::bounded(Instant::now() + Duration::from_secs(1));

        let failure = presenter
            .present_retained_seamless(ScanoutSlotId(0), 7, deadline)
            .expect_err("same-mode live flip refusal is recoverable by the caller");
        assert_eq!(failure.code, "kms-live-atomic-seamless-flip-refused");
        assert!(!presenter.has_pending_commit());
        assert_eq!(
            presenter.present(ScanoutSlotId(0), 7, deadline),
            Ok(PresentOutcome::Displayed)
        );
        assert!(presenter.io.commits[2].allow_modeset);
    }

    #[test]
    fn atomic_present_cancel_wakes_without_a_drm_event() {
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: true,
        });
        let mut presenter = presenter(io);
        presenter.cancellation.cancel(CancelScope::Generation(7));
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert!(
            presenter.io.commits.is_empty(),
            "a cancellation published before present must prevent the commit ioctl"
        );
    }

    #[test]
    fn cancel_published_during_gpu_completion_issues_no_commit() {
        let mut presenter = presenter(FakeAtomicIo::default());
        // The render integration performs bounded GPU completion before it
        // calls this seam. Publishing cancellation in that interval therefore
        // reaches present() before any atomic ioctl.
        presenter.cancellation.cancel(CancelScope::Generation(7));
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.io.commits.len(), 0);
    }

    #[test]
    fn publisher_never_blocks_even_with_a_stalled_ioctl_in_flight() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let commits = Arc::new(AtomicUsize::new(0));
        let presenter = AtomicPresenter::from_parts(
            StalledCommitIo {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                commits: Arc::clone(&commits),
            },
            selection(),
            property_ids(),
            40,
            BTreeMap::from([(ScanoutSlotId(0), 50)]),
            Arc::clone(&cancellation),
        );
        let presenting = thread::spawn(move || {
            let mut presenter = presenter;
            let outcome = presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            );
            (presenter, outcome)
        });
        entered.wait();
        assert_eq!(commits.load(Ordering::Acquire), 1);

        let (published_sender, published_receiver) = mpsc::sync_channel(1);
        let publisher_cancellation = Arc::clone(&cancellation);
        let publisher = thread::spawn(move || {
            publisher_cancellation.cancel(CancelScope::Generation(7));
            published_sender.send(()).expect("report publication");
        });
        published_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("lock-free publication completes while the ioctl is stalled");
        publisher.join().expect("publisher joins");

        release.wait();
        let (presenter, outcome) = presenting.join().expect("presenter joins");
        assert_eq!(outcome, Ok(PresentOutcome::Cancelled));
        assert!(presenter.pending_commit.is_some());
        assert_eq!(commits.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cancelled_hard_commit_failure_reports_cancelled_not_terminal() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo {
            cancel_on_commit: Some((Arc::clone(&cancellation), CancelScope::Generation(7))),
            ..Default::default()
        };
        io.commit_results.push_back(Err(commit_errno(libc::EACCES)));

        let mut presenter = presenter_with_cancellation(io, cancellation);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.io.commits.len(), 1);
        assert_eq!(presenter.pending_commit, None);
    }

    #[test]
    fn cancel_after_successful_commit_holds_the_slot_and_reports_cancelled() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let io = FakeAtomicIo {
            cancel_on_commit: Some((Arc::clone(&cancellation), CancelScope::Generation(7))),
            ..Default::default()
        };

        let mut presenter = presenter_with_cancellation(io, cancellation);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.io.commits.len(), 1);
        assert!(presenter.pending_commit_allows_modeset());
        assert_eq!(
            presenter.pending_commit,
            Some(PendingAtomicCommit {
                correlation: AtomicCommitCorrelation {
                    generation: 7,
                    slot: ScanoutSlotId(0),
                },
                allow_modeset: true,
            })
        );
    }

    #[test]
    fn cancelled_steady_flip_records_a_non_modeset_pending_commit() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let io = FakeAtomicIo {
            cancel_on_commit: Some((Arc::clone(&cancellation), CancelScope::Generation(7))),
            ..Default::default()
        };

        let mut presenter = presenter_with_cancellation(io, cancellation);
        presenter.modeset_required = false;
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert!(presenter.has_pending_commit());
        assert!(!presenter.pending_commit_allows_modeset());
    }

    #[test]
    fn teardown_after_cancelled_modeset_commit_uses_pending_commit_budget() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let io = FakeAtomicIo {
            cancel_on_commit: Some((Arc::clone(&cancellation), CancelScope::Generation(7))),
            ..Default::default()
        };
        let mut presenter = presenter_with_cancellation(io, cancellation);

        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        presenter.modeset_required = false;
        let started = Instant::now();
        let deadline = started
            + presenter.pending_commit_teardown_timeout(
                Duration::from_millis(250),
                Duration::from_millis(1_500),
            );

        assert_eq!(
            presenter.drain_pending_flip_for_teardown(deadline),
            Ok(false)
        );
        assert_eq!(presenter.io.observed_deadlines.last(), Some(&deadline));
        assert_eq!(
            deadline.duration_since(started),
            Duration::from_millis(1_500)
        );
        assert!(
            !presenter.next_present_allows_modeset(),
            "the future-request state deliberately disagrees with pending provenance"
        );
    }

    #[test]
    fn teardown_after_cancelled_steady_flip_uses_pending_commit_budget() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let io = FakeAtomicIo {
            cancel_on_commit: Some((Arc::clone(&cancellation), CancelScope::Generation(7))),
            ..Default::default()
        };
        let mut presenter = presenter_with_cancellation(io, cancellation);
        presenter.modeset_required = false;

        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        presenter.modeset_required = true;
        let started = Instant::now();
        let deadline = started
            + presenter.pending_commit_teardown_timeout(
                Duration::from_millis(250),
                Duration::from_millis(1_500),
            );

        assert_eq!(
            presenter.drain_pending_flip_for_teardown(deadline),
            Ok(false)
        );
        assert_eq!(presenter.io.observed_deadlines.last(), Some(&deadline));
        assert_eq!(deadline.duration_since(started), Duration::from_millis(250));
        assert!(
            presenter.next_present_allows_modeset(),
            "the future-request state deliberately disagrees with pending provenance"
        );
    }

    #[test]
    fn cancel_between_poll_snapshot_and_decode_wins() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        io.cancel_on_wait = Some((Arc::clone(&cancellation), CancelScope::Generation(7)));
        let mut presenter = presenter_with_cancellation(io, cancellation);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.io.events.len(), 1, "decode was never entered");
    }

    #[test]
    fn cancel_during_event_decode_wins_and_retires_the_matching_pending_commit() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        io.cancel_on_decode = Some((Arc::clone(&cancellation), CancelScope::Generation(7)));
        let mut presenter = presenter_with_cancellation(io, cancellation);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.pending_commit, None);
        assert_eq!(
            presenter.drain_pending_flip_for_teardown(Instant::now() + Duration::from_secs(1)),
            Ok(true),
            "teardown must not wait for a matching event already consumed by decode"
        );
        assert!(presenter.io.observed_deadlines.len() == 1);
    }

    #[test]
    fn seamless_pending_flip_drain_wakes_on_presentation_cancellation() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Deadline);
        let mut presenter = presenter_with_cancellation(io, Arc::clone(&cancellation));
        let first_deadline = Instant::now() + Duration::from_secs(1);
        let failure = presenter
            .present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(first_deadline),
            )
            .expect_err("deadline leaves the submitted flip pending");
        assert_eq!(failure.code, ATOMIC_PRESENT_TIMEOUT_CODE);
        assert!(presenter.has_pending_commit());

        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: true,
        });
        presenter.io.cancel_on_wait = Some((Arc::clone(&cancellation), CancelScope::Generation(7)));
        let drain_deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            presenter.drain_pending_flip(7, drain_deadline),
            Ok(PendingFlipDrainOutcome::Cancelled)
        );
        assert_eq!(
            presenter.io.observed_deadlines,
            [first_deadline, drain_deadline]
        );
    }

    #[test]
    fn atomic_present_deadline_is_absolute_and_not_rearmed() {
        let mut io = FakeAtomicIo::default();
        const STALE_WAKES: usize = 8;
        io.waits
            .extend((0..STALE_WAKES).map(|_| AtomicWaitReady::Ready {
                drm: true,
                cancel: false,
            }));
        io.waits.push_back(AtomicWaitReady::Deadline);
        io.events.extend((0..STALE_WAKES).map(|_| {
            vec![AtomicPageFlip {
                crtc_id: 999,
                tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                    generation: 7,
                    slot: ScanoutSlotId(0),
                })),
            }]
        }));
        let mut presenter = presenter(io);
        let deadline = Instant::now() + Duration::from_secs(1);
        let error = presenter
            .present(ScanoutSlotId(0), 7, PresentDeadline::bounded(deadline))
            .expect_err("stale event cannot complete present");
        assert_eq!(error.code, ATOMIC_PRESENT_TIMEOUT_CODE);
        assert_eq!(
            presenter.io.observed_deadlines,
            vec![deadline; STALE_WAKES + 1]
        );
    }

    #[test]
    fn eintr_recomputes_only_the_remaining_part_of_the_original_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        let mut times = [
            started,
            started + Duration::from_millis(30),
            started + Duration::from_millis(80),
        ]
        .into_iter();
        let mut budgets = Vec::new();
        let result = wait_with_absolute_deadline(
            deadline,
            || times.next().expect("one clock sample per EINTR arm"),
            |remaining| {
                budgets.push(remaining);
                if budgets.len() < 3 {
                    Err(io::Error::from_raw_os_error(libc::EINTR))
                } else {
                    Ok(AtomicWaitReady::Deadline)
                }
            },
        )
        .expect("EINTR is retried");
        assert_eq!(result, AtomicWaitReady::Deadline);
        assert_eq!(
            budgets,
            [
                Duration::from_millis(100),
                Duration::from_millis(70),
                Duration::from_millis(20),
            ]
        );
    }

    #[test]
    fn cancellation_wins_when_drm_and_eventfd_are_both_ready() {
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: true,
        });
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        let mut presenter = presenter(io);
        presenter.cancellation.cancel(CancelScope::Generation(7));
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(presenter.io.events.len(), 1);
    }

    #[test]
    fn stale_pageflip_cannot_complete_a_new_generation() {
        let mut io = FakeAtomicIo::default();
        io.waits.extend([
            AtomicWaitReady::Ready {
                drm: true,
                cancel: false,
            },
            AtomicWaitReady::Ready {
                drm: true,
                cancel: false,
            },
        ]);
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 8,
                slot: ScanoutSlotId(0),
            })),
        }]);
        let mut presenter = presenter(io);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                8,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Displayed)
        );
        assert_eq!(presenter.io.observed_deadlines.len(), 2);
    }

    #[test]
    fn stale_generation_cancel_is_drained_without_killing_or_spinning_a_fresh_frame() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo::default();
        io.waits.extend([
            AtomicWaitReady::Ready {
                drm: false,
                cancel: true,
            },
            AtomicWaitReady::Ready {
                drm: true,
                cancel: false,
            },
        ]);
        io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 8,
                slot: ScanoutSlotId(0),
            })),
        }]);
        io.cancel_on_wait = Some((Arc::clone(&cancellation), CancelScope::Generation(7)));
        let mut presenter = presenter_with_cancellation(io, cancellation);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            presenter.present(ScanoutSlotId(0), 8, PresentDeadline::bounded(deadline)),
            Ok(PresentOutcome::Displayed)
        );
        assert_eq!(presenter.io.observed_deadlines, [deadline, deadline]);
        assert_eq!(presenter.io.commits.len(), 1);
    }

    #[test]
    fn all_generations_stop_is_sticky_across_generation_arming() {
        let mut presenter = presenter(FakeAtomicIo::default());
        presenter.cancellation.cancel(CancelScope::AllGenerations);
        presenter.cancellation.arm_generation(99);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                99,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert!(presenter.io.commits.is_empty());
    }

    #[test]
    fn late_stale_generation_publish_cannot_overwrite_an_active_cancel() {
        let mut presenter = presenter(FakeAtomicIo::default());
        presenter.cancellation.cancel(CancelScope::Generation(8));
        presenter.cancellation.cancel(CancelScope::Generation(7));

        assert_eq!(presenter.cancellation.generation.load(Ordering::Acquire), 8);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                8,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert!(presenter.io.commits.is_empty());
    }

    #[test]
    fn arm_generation_clears_only_a_non_matching_publication() {
        let mut matching = presenter(FakeAtomicIo::default());
        matching.cancellation.cancel(CancelScope::Generation(8));
        matching.cancellation.arm_generation(8);
        assert_eq!(
            matching.present(
                ScanoutSlotId(0),
                8,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert!(matching.io.commits.is_empty());

        let mut stale = presenter(FakeAtomicIo::default());
        stale.cancellation.cancel(CancelScope::Generation(7));
        stale.cancellation.arm_generation(8);
        stale.io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        stale.io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 8,
                slot: ScanoutSlotId(0),
            })),
        }]);
        assert_eq!(
            stale.present(
                ScanoutSlotId(0),
                8,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Displayed)
        );
    }

    #[test]
    fn admission_and_live_commit_share_the_exact_property_set_builder() {
        let mut presenter = presenter(FakeAtomicIo::default());
        presenter
            .admission_probe(ScanoutSlotId(0), 7, Instant::now() + Duration::from_secs(1))
            .expect("test-only admission");
        assert_eq!(
            presenter.io.commits,
            [
                AtomicCommitOptions {
                    test_only: true,
                    allow_modeset: true,
                    nonblock: false,
                    page_flip_event: false,
                    correlation: None,
                },
                AtomicCommitOptions {
                    test_only: true,
                    allow_modeset: true,
                    nonblock: true,
                    page_flip_event: false,
                    correlation: None,
                },
            ]
        );
        assert_eq!(presenter.io.requests[0], presenter.io.requests[1]);
        presenter
            .io
            .commit_results
            .extend([Err(commit_errno(libc::EBUSY)), Ok(())]);
        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: false,
        });
        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        presenter.io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        presenter
            .present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            )
            .expect("first live commit");
        assert_eq!(presenter.io.requests[0], presenter.io.requests[2]);
    }

    fn commit_errno(errno: i32) -> AtomicCommitError {
        AtomicCommitError {
            operation: "fake atomic commit",
            errno: Some(errno),
            detail: io::Error::from_raw_os_error(errno).to_string(),
        }
    }

    #[test]
    fn admission_retries_ebusy_within_its_original_deadline() {
        let mut io = FakeAtomicIo::default();
        io.commit_results
            .extend([Err(commit_errno(libc::EBUSY)), Ok(()), Ok(())]);
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: false,
        });
        let mut presenter = presenter(io);
        let deadline = Instant::now() + Duration::from_secs(1);
        presenter
            .admission_probe(ScanoutSlotId(0), 7, deadline)
            .expect("bounded EBUSY retry succeeds");
        assert_eq!(presenter.io.observed_deadlines, [deadline]);
        assert_eq!(presenter.io.commits.len(), 3);
    }

    #[test]
    fn admission_hard_rejection_preserves_errno_and_name() {
        let mut io = FakeAtomicIo::default();
        io.commit_results.push_back(Err(commit_errno(libc::EINVAL)));
        let error = presenter(io)
            .admission_probe(ScanoutSlotId(0), 7, Instant::now() + Duration::from_secs(1))
            .expect_err("EINVAL is a hard admission rejection");
        assert_eq!(error.code, "kms-live-atomic-admission-hard-rejection");
        assert_eq!(
            error.commit.as_ref().and_then(|commit| commit.errno),
            Some(libc::EINVAL)
        );
        assert!(error.detail.contains(&format!("errno {}", libc::EINVAL)));
    }

    #[test]
    fn first_live_nonblocking_modeset_is_the_named_acceptance_gate() {
        let mut io = FakeAtomicIo::default();
        io.commit_results.push_back(Err(commit_errno(libc::EINVAL)));
        let error = presenter(io)
            .present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            )
            .expect_err("first live NONBLOCK modeset is rejected");
        assert_eq!(
            error.code,
            "kms-live-atomic-first-nonblocking-modeset-refused"
        );
        assert!(error.detail.contains(&format!("errno {}", libc::EINVAL)));
    }

    #[test]
    fn first_live_nonblocking_modeset_retries_ebusy_then_names_deadline() {
        let mut io = FakeAtomicIo::default();
        io.commit_results.push_back(Err(commit_errno(libc::EBUSY)));
        io.waits.push_back(AtomicWaitReady::Deadline);
        let error = presenter(io)
            .present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            )
            .expect_err("EBUSY persists through its bounded wait");
        assert_eq!(
            error.code,
            "kms-live-atomic-first-nonblocking-modeset-busy-deadline"
        );
        assert!(error.detail.contains(&format!("errno {}", libc::EBUSY)));
    }

    #[test]
    fn teardown_drains_an_in_flight_cancelled_flip_before_disable() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let mut io = FakeAtomicIo::default();
        io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: true,
        });
        io.cancel_on_wait = Some((Arc::clone(&cancellation), CancelScope::Generation(7)));
        let mut presenter = presenter_with_cancellation(io, cancellation);
        assert_eq!(
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(1)),
            ),
            Ok(PresentOutcome::Cancelled)
        );
        assert_eq!(
            presenter.pending_commit,
            Some(PendingAtomicCommit {
                correlation: AtomicCommitCorrelation {
                    generation: 7,
                    slot: ScanoutSlotId(0),
                },
                allow_modeset: true,
            })
        );

        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        presenter.io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Presentation(AtomicCommitCorrelation {
                generation: 7,
                slot: ScanoutSlotId(0),
            })),
        }]);
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            presenter.drain_pending_flip_for_teardown(deadline),
            Ok(true)
        );
        assert_eq!(presenter.pending_commit, None);

        presenter
            .io
            .commit_results
            .extend([Err(commit_errno(libc::EBUSY)), Ok(())]);
        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: false,
            cancel: false,
        });
        presenter.io.waits.push_back(AtomicWaitReady::Ready {
            drm: true,
            cancel: false,
        });
        presenter.io.events.push_back(vec![AtomicPageFlip {
            crtc_id: 20,
            tag: Some(AtomicPageFlipTag::Disable),
        }]);
        presenter
            .disable_nonblocking(deadline)
            .expect("disable follows drained flip");
        assert_eq!(presenter.io.commits.len(), 3);
    }

    #[test]
    fn kernel_user_data_routes_out_of_order_pageflips_without_fifo_fabrication() {
        let first = AtomicCommitCorrelation {
            generation: 41,
            slot: ScanoutSlotId(0),
        };
        let second = AtomicCommitCorrelation {
            generation: 42,
            slot: ScanoutSlotId(1),
        };
        let state = Mutex::new(AtomicEventRouterState {
            next_token: 23,
            pending: BTreeMap::from([
                (11, AtomicPageFlipTag::Presentation(first)),
                (22, AtomicPageFlipTag::Presentation(second)),
            ]),
            completed: BTreeMap::new(),
        });
        let events = [
            DrmEventVblank {
                header: DrmEventHeader {
                    kind: DRM_EVENT_FLIP_COMPLETE,
                    length: std::mem::size_of::<DrmEventVblank>() as u32,
                },
                user_data: 22,
                tv_sec: 0,
                tv_usec: 0,
                sequence: 1,
                crtc_id: 202,
            },
            DrmEventVblank {
                header: DrmEventHeader {
                    kind: DRM_EVENT_FLIP_COMPLETE,
                    length: std::mem::size_of::<DrmEventVblank>() as u32,
                },
                user_data: 11,
                tv_sec: 0,
                tv_usec: 0,
                sequence: 2,
                crtc_id: 101,
            },
        ];
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), std::mem::size_of_val(&events))
        };
        assert_eq!(
            decode_raw_pageflips(bytes, &state).expect("raw pageflip decode"),
            [
                AtomicPageFlip {
                    crtc_id: 202,
                    tag: Some(AtomicPageFlipTag::Presentation(second)),
                },
                AtomicPageFlip {
                    crtc_id: 101,
                    tag: Some(AtomicPageFlipTag::Presentation(first)),
                },
            ]
        );
        assert!(state.lock().expect("router state").pending.is_empty());
    }

    #[test]
    fn sole_event_reader_retains_another_crtcs_completion_for_its_presenter() {
        let state = Mutex::new(AtomicEventRouterState {
            next_token: 1,
            pending: BTreeMap::new(),
            completed: BTreeMap::new(),
        });
        let first = AtomicPageFlip {
            crtc_id: 101,
            tag: Some(AtomicPageFlipTag::Disable),
        };
        let second = AtomicPageFlip {
            crtc_id: 202,
            tag: Some(AtomicPageFlipTag::Disable),
        };
        assert_eq!(
            route_pageflips(&state, [second, first], 101).expect("route first CRTC"),
            [first]
        );
        assert_eq!(
            take_routed_pageflips(&state, 202).expect("take retained second CRTC"),
            [second]
        );
    }

    #[test]
    fn one_event_router_is_shared_by_every_presenter_for_a_device_queue() {
        fn eventfd() -> OwnedFd {
            let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            assert!(raw >= 0);
            unsafe { OwnedFd::from_raw_fd(raw) }
        }

        let router = ProductionAtomicEventRouter::new(eventfd()).expect("event router");
        let first = ProductionAtomicIo::new(eventfd(), Arc::clone(&router));
        let second = ProductionAtomicIo::new(eventfd(), Arc::clone(&router));
        assert!(Arc::ptr_eq(&first.events, &second.events));
        assert!(Arc::ptr_eq(&first.events, &router));
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn raw_atomic_ioctl_number_matches_linux_drm_mode_atomic_layout() {
        assert_eq!(std::mem::size_of::<DrmModeAtomic>(), 56);
        assert_eq!(drm_ioctl_mode_atomic(), 0xc038_64bc);
    }

    #[test]
    fn rmfb_failure_retains_the_kernel_id_for_fail_closed_ownership() {
        let mut presenter = presenter(FakeAtomicIo::default());
        presenter
            .io
            .remove_results
            .push_back(Err("injected RmFB EBUSY".into()));
        let states = [(ScanoutSlotId(0), ScanoutSlotState::Free)];
        assert_eq!(
            presenter.remove_framebuffers(&states, false),
            Err("injected RmFB EBUSY".into())
        );
        assert_eq!(presenter.framebuffer(ScanoutSlotId(0)), Some(50));
        presenter.io.remove_results.push_back(Ok(()));
        presenter
            .remove_framebuffers(&states, false)
            .expect("retained framebuffer can be retried");
        assert_eq!(presenter.framebuffer(ScanoutSlotId(0)), None);
    }

    #[test]
    fn rmfb_refuses_a_live_slot_without_revoked_authority() {
        for state in [ScanoutSlotState::Queued, ScanoutSlotState::Front] {
            let mut presenter = presenter(FakeAtomicIo::default());
            let error = presenter
                .remove_framebuffers(&[(ScanoutSlotId(0), state)], false)
                .expect_err("live scanout framebuffer cannot be removed");
            assert!(error.contains("kms-live-atomic-rmfb-live-slot-refused"));
            assert!(presenter.io.removed_framebuffers.is_empty());
            assert_eq!(presenter.framebuffer(ScanoutSlotId(0)), Some(50));
        }
    }

    #[test]
    fn revoked_authority_permits_fail_closed_rmfb_attempt_of_a_live_slot() {
        let mut presenter = presenter(FakeAtomicIo::default());
        presenter
            .remove_framebuffers(&[(ScanoutSlotId(0), ScanoutSlotState::Front)], true)
            .expect("revoked authority ends the kernel scanout lifetime");
        assert_eq!(presenter.io.removed_framebuffers, [50]);
        assert_eq!(presenter.framebuffer(ScanoutSlotId(0)), None);
    }

    struct BlockingCancelIo;

    impl AtomicIo for BlockingCancelIo {
        fn add_framebuffer(
            &mut self,
            _slot: ScanoutSlotId,
            _buffer: &dyn PlanarBuffer,
        ) -> Result<u32, String> {
            Ok(50)
        }

        fn remove_framebuffer(&mut self, _framebuffer: u32) -> Result<(), String> {
            Ok(())
        }

        fn commit(
            &mut self,
            _request: &AtomicRequest,
            _options: AtomicCommitOptions,
        ) -> Result<(), AtomicCommitError> {
            Ok(())
        }

        fn wait_ready(
            &mut self,
            _crtc_id: u32,
            cancel: BorrowedFd<'_>,
            absolute_deadline: Instant,
        ) -> Result<AtomicWaitReady, String> {
            let remaining = absolute_deadline.saturating_duration_since(Instant::now());
            let mut descriptor = libc::pollfd {
                fd: cancel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
            let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            Ok(if ready > 0 {
                AtomicWaitReady::Ready {
                    drm: false,
                    cancel: true,
                }
            } else {
                AtomicWaitReady::Deadline
            })
        }

        fn decode_pageflips(&mut self, _crtc_id: u32) -> Result<Vec<AtomicPageFlip>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn stop_wakes_a_blocked_presenter_without_the_pump_mailbox() {
        let cancellation = AtomicCancellation::new().expect("eventfd");
        let cancel = cancellation.handle();
        let mut presenter = AtomicPresenter::from_parts(
            BlockingCancelIo,
            selection(),
            property_ids(),
            40,
            BTreeMap::from([(ScanoutSlotId(0), 50)]),
            cancellation,
        );
        let blocked = std::thread::spawn(move || {
            presenter.present(
                ScanoutSlotId(0),
                7,
                PresentDeadline::bounded(Instant::now() + Duration::from_secs(2)),
            )
        });
        cancel.cancel(CancelScope::AllGenerations);
        assert_eq!(
            blocked.join().expect("presenter thread"),
            Ok(PresentOutcome::Cancelled)
        );
    }
}
