//!
//! Implementation of the [`Session`] trait through the libseat.
//!
//! This requires libseat to be available on the system.

use libseat::{Seat, SeatEvent};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    os::unix::io::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use rustix::{fs::OFlags, io::Errno};

use calloop::{
    channel::{self, Channel},
    EventSource, Poll, PostAction, Readiness, Token, TokenFactory,
};

use crate::backend::session::{AsErrno, Event as SessionEvent, Session};

use tracing::{debug, error, info_span, instrument};

#[derive(Debug)]
struct LibSeatSessionImpl {
    seat: RefCell<Seat>,
    active: Arc<AtomicBool>,
    opens_refused: Arc<AtomicBool>,
    devices: RefCell<HashMap<RawFd, libseat::Device>>,
}

impl Drop for LibSeatSessionImpl {
    fn drop(&mut self) {
        debug!("Closing seat")
    }
}

/// [`Session`] via the libseat
#[derive(Debug, Clone)]
pub struct LibSeatSession {
    internal: Weak<LibSeatSessionImpl>,
    seat_name: String,
    span: tracing::Span,
}

/// `SessionNotifier` via the libseat
#[derive(Debug)]
pub struct LibSeatSessionNotifier {
    internal: Rc<LibSeatSessionImpl>,
    rx: Channel<SeatEvent>,
    token: Option<Token>,
    span: tracing::Span,
}

/// `SessionNotifier` variant which defers libseat's disable acknowledgement.
///
/// Unlike [`LibSeatSessionNotifier`], a raw disable first produces
/// [`DeferredSessionEvent::PauseRequested`]. Device authority may already have
/// been revoked by the libseat backend before that callback; only the later
/// protocol-level `seat.disable()` acknowledgement is deferred. The notifier
/// sends that acknowledgement after the request is answered, its acknowledger
/// disappears, or the bounded wait expires. Waiting happens on a helper thread
/// so this event source keeps dispatching cleanup requests meanwhile.
#[derive(Debug)]
pub struct DeferredLibSeatSessionNotifier {
    internal: Rc<LibSeatSessionImpl>,
    rx: Channel<SeatEvent>,
    resolutions: Channel<DeferredDisableResolution>,
    resolution_tx: channel::Sender<DeferredDisableResolution>,
    acknowledgement_timeout: Duration,
    phase: DeferredDisablePhase,
    next_request: u64,
    token: Option<Token>,
    span: tracing::Span,
}

#[derive(Debug)]
enum DeferredDisablePhase {
    Active,
    Awaiting { request: u64, activate_pending: bool },
    Disabled,
}

#[derive(Debug)]
struct DeferredDisableResolution {
    request: u64,
    acknowledgement: DeferredDisableAcknowledgementOutcome,
}

/// The one-shot answer to a deferred libseat disable request.
pub struct DeferredDisableAcknowledgement(Option<mpsc::SyncSender<()>>);

impl DeferredDisableAcknowledgement {
    /// Permit the notifier to call `seat.disable()`.
    ///
    /// Returns false only when the bounded waiter has already gone away. The
    /// notifier still disables the seat in that case.
    pub fn acknowledge(mut self) -> bool {
        self.0.take().is_some_and(|sender| sender.send(()).is_ok())
    }
}

impl fmt::Debug for DeferredDisableAcknowledgement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeferredDisableAcknowledgement")
            .field("pending", &self.0.is_some())
            .finish()
    }
}

/// How the bounded protocol-acknowledgement wait ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredDisableAcknowledgementOutcome {
    /// The coordinator explicitly released the seat.
    Acknowledged,
    /// The coordinator did not answer before the configured deadline.
    TimedOut,
    /// Every acknowledgement handle was dropped without answering.
    AcknowledgerGone,
}

/// Result reported after the notifier has attempted `seat.disable()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeferredDisableOutcome {
    /// How the protocol-acknowledgement wait ended.
    pub acknowledgement: DeferredDisableAcknowledgementOutcome,
    /// Whether libseat accepted the disable acknowledgement.
    pub disable_succeeded: bool,
}

impl DeferredDisableOutcome {
    /// True only for the resumable, coordinator-acknowledged path.
    pub fn resumable(self) -> bool {
        self.acknowledgement == DeferredDisableAcknowledgementOutcome::Acknowledged && self.disable_succeeded
    }
}

/// Events emitted by [`DeferredLibSeatSessionNotifier`].
#[derive(Debug)]
pub enum DeferredSessionEvent {
    /// The session became active.
    ActivateSession,
    /// The backend requested its protocol acknowledgement after revoking authority.
    PauseRequested {
        /// One-shot permission to proceed with `seat.disable()`.
        acknowledgement: DeferredDisableAcknowledgement,
    },
    /// `seat.disable()` has been attempted after the bounded wait.
    Paused {
        /// Whether the operation remains resumable.
        outcome: DeferredDisableOutcome,
    },
}

impl LibSeatSession {
    /// Tries to create a new session via libseat.
    pub fn new() -> Result<(LibSeatSession, LibSeatSessionNotifier), Error> {
        let span = info_span!("backend_session", "type" = "libseat");
        let _guard = span.enter();
        let (tx, rx) = calloop::channel::channel();

        let seat = {
            Seat::open(move |_seat, event| match event {
                SeatEvent::Enable => {
                    debug!("Enable callback called");
                    tx.send(event).unwrap();
                }
                SeatEvent::Disable => {
                    debug!("Disable callback called");
                    tx.send(event).unwrap();
                }
            })
        };

        drop(_guard);
        seat.map(|mut seat| {
            let seat_name = seat.name().to_owned();

            // In some cases enable_seat event is avalible right after startup
            // so, we can dispatch it
            seat.dispatch(0).unwrap();
            let active = matches!(rx.try_recv(), Ok(SeatEvent::Enable));

            let internal = Rc::new(LibSeatSessionImpl {
                seat: RefCell::new(seat),
                active: Arc::new(AtomicBool::new(active)),
                opens_refused: Arc::new(AtomicBool::new(false)),
                devices: RefCell::new(HashMap::new()),
            });

            let session = LibSeatSession {
                internal: Rc::downgrade(&internal),
                seat_name,
                span: span.clone(),
            };

            let notifier = LibSeatSessionNotifier {
                internal,
                rx,
                token: None,
                span,
            };

            (session, notifier)
        })
        .map_err(|err| Error::FailedToOpenSession(Errno::from_raw_os_error(err.into())))
    }

    /// Tries to create a libseat session whose protocol-level disable
    /// acknowledgement is deferred while the compositor performs bounded cleanup.
    pub fn new_with_deferred_disable(
        acknowledgement_timeout: Duration,
    ) -> Result<(LibSeatSession, DeferredLibSeatSessionNotifier), Error> {
        let span = info_span!("backend_session", "type" = "libseat-deferred-disable");
        let _guard = span.enter();
        let (tx, rx) = calloop::channel::channel();
        let active = Arc::new(AtomicBool::new(false));
        let opens_refused = Arc::new(AtomicBool::new(true));
        let callback_active = active.clone();
        let callback_refusal = opens_refused.clone();

        let seat = Seat::open(move |_seat, event| {
            match event {
                SeatEvent::Enable => {
                    debug!("Deferred enable callback called");
                }
                SeatEvent::Disable => {
                    debug!("Deferred disable callback called");
                    // This store precedes publication of the request. Even if
                    // the coordinator or its mailbox has vanished, no later
                    // open can slip into the cleanup-before-acknowledgement window.
                    callback_active.store(false, Ordering::SeqCst);
                    callback_refusal.store(true, Ordering::SeqCst);
                }
            }
            let _ = tx.send(event);
        });

        drop(_guard);
        seat.map(|mut seat| {
            let seat_name = seat.name().to_owned();
            seat.dispatch(0).unwrap();
            let internal = Rc::new(LibSeatSessionImpl {
                seat: RefCell::new(seat),
                active,
                opens_refused,
                devices: RefCell::new(HashMap::new()),
            });
            let (resolution_tx, resolutions) = calloop::channel::channel();
            let session = LibSeatSession {
                internal: Rc::downgrade(&internal),
                seat_name,
                span: span.clone(),
            };
            let notifier = DeferredLibSeatSessionNotifier {
                internal,
                rx,
                resolutions,
                resolution_tx,
                acknowledgement_timeout,
                // Any initial Enable remains queued in `rx` and commits the
                // active state through `activate`. This also means an Enable
                // racing a pending disable cannot reopen the device gate
                // before `seat.disable()` has run.
                phase: DeferredDisablePhase::Disabled,
                next_request: 1,
                token: None,
                span,
            };
            (session, notifier)
        })
        .map_err(|err| Error::FailedToOpenSession(Errno::from_raw_os_error(err.into())))
    }
}

impl Session for LibSeatSession {
    type Error = Error;

    #[instrument(parent = &self.span, skip(self))]
    fn open(&mut self, path: &Path, _flags: OFlags) -> Result<OwnedFd, Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            if session.opens_refused.load(Ordering::SeqCst) {
                return Err(Error::SessionInactive);
            }
            debug!("Opening device: {:?}", path);

            session
                .seat
                .borrow_mut()
                .open_device(&path)
                .map(|device| {
                    let raw_fd = device.as_fd().as_raw_fd();

                    session.devices.borrow_mut().insert(raw_fd, device);

                    // SAFETY: `libseat::Device` does not close fd on drop
                    unsafe { OwnedFd::from_raw_fd(raw_fd) }
                })
                .map_err(|err| Error::FailedToOpenDevice(Errno::from_raw_os_error(err.into())))
        } else {
            Err(Error::SessionLost)
        }
    }

    #[instrument(parent = &self.span, skip(self))]
    fn close(&mut self, fd: OwnedFd) -> Result<(), Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            debug!("Closing device: {:?}", fd);

            let out = if let Some(dev) = session.devices.borrow_mut().remove(&fd.as_fd().as_raw_fd()) {
                session
                    .seat
                    .borrow_mut()
                    .close_device(dev)
                    .map_err(|err| Error::FailedToCloseDevice(Errno::from_raw_os_error(err.into())))
            } else {
                Ok(())
            };

            // `fd` is closed on drop

            out
        } else {
            Err(Error::SessionLost)
        }
    }

    #[instrument(parent = &self.span, skip(self))]
    fn change_vt(&mut self, vt: i32) -> Result<(), Self::Error> {
        if let Some(session) = self.internal.upgrade() {
            debug!("Session switch: {:?}", vt);
            session
                .seat
                .borrow_mut()
                .switch_session(vt)
                .map_err(|err| Error::FailedToChangeVt(Errno::from_raw_os_error(err.into())))
        } else {
            Err(Error::SessionLost)
        }
    }

    fn is_active(&self) -> bool {
        if let Some(internal) = self.internal.upgrade() {
            internal.active.load(Ordering::SeqCst)
        } else {
            false
        }
    }

    fn seat(&self) -> String {
        self.seat_name.clone()
    }
}

impl LibSeatSessionNotifier {
    /// Creates a new session object belonging to this notifier.
    pub fn session(&self) -> LibSeatSession {
        LibSeatSession {
            internal: Rc::downgrade(&self.internal),
            seat_name: self.internal.seat.borrow_mut().name().to_owned(),
            span: self.span.clone(),
        }
    }
}

impl EventSource for LibSeatSessionNotifier {
    type Event = SessionEvent;
    type Metadata = ();
    type Ret = ();
    type Error = Error;

    #[profiling::function]
    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, Error>
    where
        F: FnMut(SessionEvent, &mut ()),
    {
        if Some(token) == self.token {
            self.internal.seat.borrow_mut().dispatch(0).unwrap();
        }

        let internal = &self.internal;
        self.rx
            .process_events(readiness, token, |event, _| match event {
                channel::Event::Msg(event) => match event {
                    SeatEvent::Enable => {
                        internal.active.store(true, Ordering::SeqCst);
                        callback(SessionEvent::ActivateSession, &mut ());
                    }
                    SeatEvent::Disable => {
                        internal.active.store(false, Ordering::SeqCst);
                        internal.seat.borrow_mut().disable().unwrap();
                        callback(SessionEvent::PauseSession, &mut ());
                    }
                },
                channel::Event::Closed => {
                    // Tx is stored inside of Seat, and Rc<Seat> is stored in LibSeatSessionNotifier so this is unreachable
                }
            })
            .map_err(|_| Error::SessionLost)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.register(poll, factory)?;

        self.token = Some(factory.token());
        let mut seat = self.internal.seat.borrow_mut();
        // Safety: the seat fd cannot be close without removing the LibSeatSessionNotifier from the event loop
        unsafe {
            poll.register(
                seat.get_fd().unwrap(),
                calloop::Interest::READ,
                calloop::Mode::Level,
                self.token.unwrap(),
            )
        }
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.reregister(poll, factory)?;

        self.token = Some(factory.token());
        let mut seat = self.internal.seat.borrow_mut();
        poll.reregister(
            seat.get_fd().unwrap(),
            calloop::Interest::READ,
            calloop::Mode::Level,
            self.token.unwrap(),
        )
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.rx.unregister(poll)?;

        self.token = None;
        let mut seat = self.internal.seat.borrow_mut();
        poll.unregister(seat.get_fd().unwrap())
    }
}

impl DeferredLibSeatSessionNotifier {
    fn request_pause<F>(&mut self, callback: &mut F)
    where
        F: FnMut(DeferredSessionEvent, &mut ()),
    {
        if !matches!(self.phase, DeferredDisablePhase::Active) {
            return;
        }
        let request = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        self.phase = DeferredDisablePhase::Awaiting {
            request,
            activate_pending: false,
        };
        let (acknowledgement, answer) = mpsc::sync_channel(1);
        let resolution_tx = self.resolution_tx.clone();
        let fallback_tx = self.resolution_tx.clone();
        let timeout = self.acknowledgement_timeout;
        if thread::Builder::new()
            .name("smithay-seat-disable-ack".into())
            .spawn(move || {
                let acknowledgement = match answer.recv_timeout(timeout) {
                    Ok(()) => DeferredDisableAcknowledgementOutcome::Acknowledged,
                    Err(mpsc::RecvTimeoutError::Timeout) => DeferredDisableAcknowledgementOutcome::TimedOut,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        DeferredDisableAcknowledgementOutcome::AcknowledgerGone
                    }
                };
                let _ = resolution_tx.send(DeferredDisableResolution {
                    request,
                    acknowledgement,
                });
            })
            .is_err()
        {
            let _ = fallback_tx.send(DeferredDisableResolution {
                request,
                acknowledgement: DeferredDisableAcknowledgementOutcome::AcknowledgerGone,
            });
        }
        callback(
            DeferredSessionEvent::PauseRequested {
                acknowledgement: DeferredDisableAcknowledgement(Some(acknowledgement)),
            },
            &mut (),
        );
    }

    fn activate<F>(&mut self, callback: &mut F)
    where
        F: FnMut(DeferredSessionEvent, &mut ()),
    {
        match &mut self.phase {
            DeferredDisablePhase::Active => {
                self.internal.active.store(true, Ordering::SeqCst);
                self.internal.opens_refused.store(false, Ordering::SeqCst);
                callback(DeferredSessionEvent::ActivateSession, &mut ());
            }
            DeferredDisablePhase::Awaiting { activate_pending, .. } => *activate_pending = true,
            DeferredDisablePhase::Disabled => {
                self.phase = DeferredDisablePhase::Active;
                self.internal.active.store(true, Ordering::SeqCst);
                self.internal.opens_refused.store(false, Ordering::SeqCst);
                callback(DeferredSessionEvent::ActivateSession, &mut ());
            }
        }
    }

    fn finish_pause<F>(&mut self, resolution: DeferredDisableResolution, callback: &mut F)
    where
        F: FnMut(DeferredSessionEvent, &mut ()),
    {
        let DeferredDisablePhase::Awaiting {
            request,
            activate_pending,
        } = self.phase
        else {
            return;
        };
        if request != resolution.request {
            return;
        }
        let disable_succeeded = match self.internal.seat.borrow_mut().disable() {
            Ok(()) => true,
            Err(error) => {
                error!(%error, "Deferred libseat disable acknowledgement failed");
                false
            }
        };
        self.phase = DeferredDisablePhase::Disabled;
        callback(
            DeferredSessionEvent::Paused {
                outcome: DeferredDisableOutcome {
                    acknowledgement: resolution.acknowledgement,
                    disable_succeeded,
                },
            },
            &mut (),
        );
        if activate_pending {
            self.activate(callback);
        }
    }
}

impl EventSource for DeferredLibSeatSessionNotifier {
    type Event = DeferredSessionEvent;
    type Metadata = ();
    type Ret = ();
    type Error = Error;

    #[profiling::function]
    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, Error>
    where
        F: FnMut(DeferredSessionEvent, &mut ()),
    {
        let span = self.span.clone();
        let _guard = span.enter();
        if Some(token) == self.token {
            self.internal.seat.borrow_mut().dispatch(0).unwrap();
        }

        let mut seat_events = Vec::new();
        self.rx
            .process_events(readiness, token, |event, _| {
                if let channel::Event::Msg(event) = event {
                    seat_events.push(event);
                }
            })
            .map_err(|_| Error::SessionLost)?;
        for event in seat_events {
            match event {
                SeatEvent::Enable => self.activate(&mut callback),
                SeatEvent::Disable => self.request_pause(&mut callback),
            }
        }

        let mut resolutions = Vec::new();
        self.resolutions
            .process_events(readiness, token, |event, _| {
                if let channel::Event::Msg(resolution) = event {
                    resolutions.push(resolution);
                }
            })
            .map_err(|_| Error::SessionLost)?;
        for resolution in resolutions {
            self.finish_pause(resolution, &mut callback);
        }
        Ok(PostAction::Continue)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.register(poll, factory)?;
        self.resolutions.register(poll, factory)?;
        self.token = Some(factory.token());
        let mut seat = self.internal.seat.borrow_mut();
        // Safety: the seat fd cannot be closed without removing this notifier
        // from the event loop.
        unsafe {
            poll.register(
                seat.get_fd().unwrap(),
                calloop::Interest::READ,
                calloop::Mode::Level,
                self.token.unwrap(),
            )
        }
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.rx.reregister(poll, factory)?;
        self.resolutions.reregister(poll, factory)?;
        self.token = Some(factory.token());
        let mut seat = self.internal.seat.borrow_mut();
        poll.reregister(
            seat.get_fd().unwrap(),
            calloop::Interest::READ,
            calloop::Mode::Level,
            self.token.unwrap(),
        )
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.rx.unregister(poll)?;
        self.resolutions.unregister(poll)?;
        self.token = None;
        let mut seat = self.internal.seat.borrow_mut();
        poll.unregister(seat.get_fd().unwrap())
    }
}

/// Errors related to direct/tty sessions
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Failed to open session
    #[error("Failed to open session: {0}")]
    FailedToOpenSession(Errno),

    /// Failed to open device
    #[error("Failed to open device: {0}")]
    FailedToOpenDevice(Errno),

    /// Failed to close device
    #[error("Failed to close device: {0}")]
    FailedToCloseDevice(Errno),

    /// Failed to close device
    #[error("Failed to change vt: {0}")]
    FailedToChangeVt(Errno),

    /// Session is already closed,
    #[error("Session is already closed")]
    SessionLost,

    /// The deferred-disable notifier has refused new device opens.
    #[error("Session is inactive while disable is pending")]
    SessionInactive,
}

impl AsErrno for Error {
    fn as_errno(&self) -> Option<i32> {
        match self {
            &Self::FailedToOpenSession(errno)
            | &Self::FailedToOpenDevice(errno)
            | &Self::FailedToCloseDevice(errno)
            | &Self::FailedToChangeVt(errno) => Some(errno.raw_os_error()),
            Self::SessionInactive => Some(Errno::ACCESS.raw_os_error()),
            _ => None,
        }
    }
}
