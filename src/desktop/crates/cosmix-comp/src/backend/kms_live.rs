//! Fail-closed authorisation and ownership for the sealed live KMS entry point.
//!
//! The pure decision core consumes injected facts. Production authorisation is
//! compiled only with `kms-live`, owns the verified controlling-TTY alias, and
//! returns a non-transferable grant that must be revalidated immediately before
//! any destructive act. The real body remains unavailable to test binaries;
//! its orchestration runs through inert `LiveActPlatform` implementations.
//!
//! An external libseat disable callback is evidence that device authority has
//! already gone, not a protected pre-revocation window. seatd revokes DRM and
//! evdev devices before sending the disable event, and the logind backend
//! acknowledges its pause immediately after the callback returns. The deferred
//! notifier therefore orders only local cleanup before the protocol-level
//! acknowledgement: bounded render suspend-or-detach, input reconciliation and
//! close, original DRM-fd close, acknowledgement, then `Paused`/resume when the
//! render worker quiesced cleanly.

use crate::decoration::DecorationStartup;
use cosmix_deco::ChromeStyle;
use std::{
    collections::BTreeSet,
    error::Error,
    ffi::OsString,
    fmt,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::PathBuf,
    rc::Rc,
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[cfg(all(feature = "kms-live", not(test)))]
use std::{
    sync::mpsc::{self, Receiver, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

// The live build imports these above. The offline build needs them too: the
// revocation drain, the shutdown-ask classification and the input-open refusal
// are compiled under tests so their boundaries can be exercised against real
// channels rather than described in a comment.
#[cfg(test)]
use std::{
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

#[cfg(all(feature = "kms-live", not(test)))]
#[cfg(all(feature = "kms-live", not(test)))]
use smithay::backend::libinput::LibinputInputBackend;
#[cfg(test)]
use smithay::backend::session::libseat::DeferredDisableAcknowledgementOutcome;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
use smithay::backend::session::libseat::DeferredDisableOutcome;
#[cfg(all(feature = "kms-live", not(test)))]
use smithay::backend::session::{
    AsErrno, Session,
    libseat::{DeferredSessionEvent, LibSeatSession},
};
#[cfg(all(feature = "kms-live", not(test)))]
use smithay::reexports::input::Libinput;

#[cfg(all(feature = "kms-live", not(test)))]
use crate::protocol::{BoxedLibinputFactory, InputSourceFactory};
#[cfg(any(all(feature = "kms-live", not(test)), test))]
use smithay::reexports::calloop::{EventLoop, channel};
#[cfg(all(feature = "kms-live", not(test)))]
use smithay::{
    backend::udev::{UdevBackend, UdevEvent},
    reexports::rustix::{self, fs::OFlags},
};

#[cfg(any(feature = "kms-live", test))]
use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    path::Path,
};

#[cfg(all(feature = "kms-live", not(test)))]
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use super::kms::{KmsRenderCommand, KmsRenderReply, OutputKey, SelectedOutput};
use super::kms::{OutputScale120, PresentationBackend};
#[cfg(all(feature = "kms-live", not(test)))]
use super::libinput_live::{
    ForwardingLibinputInterface, InputOpenGate, InputOpenRefusal, LibinputDeviceTransport,
    authorise_input_open, deliver_open_reply, ensure_close_on_exec, input_open_flags,
    input_open_reply_channel, observe_node, verify_opened_input_node,
};
// The one type `refuse_input_open_after_wait_failure` needs that the offline
// build does not otherwise import.
#[cfg(test)]
use super::libinput_live::InputOpenGate;
use super::render::LiveSceneMode;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
use super::render::{KmsRenderFrameEvent, LiveOutputRegistration, PumpReply};
#[cfg(all(feature = "kms-live", not(test)))]
use super::resume_scanout::{
    ResumeModesetReason, ResumePresentationClassification, ResumeScanoutSnapshot,
    classify_resume_scanout,
};
#[cfg(all(feature = "kms-live", not(test)))]
use super::scan::{
    ConnectorProbe, ConnectorStatus, DrmMasterState, borrowed_master_state, scan_borrowed_card,
};

const KMS_LIVE_SUBCOMMAND: &str = "kms-live";
const LINUX_VT_MAJOR: u32 = 4;
const MIN_LINUX_VT: u32 = 1;
const MAX_LINUX_VT: u32 = 63;
const TTYAUX_MAJOR: u32 = 5;
const TTY_ALIAS_MINOR: u32 = 0;
const CONFIRMATION_NONCE_BYTES: usize = 4;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const NO_SUBMIT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_PUMP_PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_TOPOLOGY_ACK_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_PREPARATION_MAILBOX_SLICE: Duration = Duration::from_millis(10);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_INPUT_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const SELF_SWITCH_PAUSE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_RESUME_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const EXTERNAL_PAUSE_ACK_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const EXTERNAL_PAUSED_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(all(feature = "kms-live", not(test)), test))]
const LIVE_RESUME_BACKOFFS: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(100)];
#[cfg(any(feature = "kms-live", test))]
const SELF_SWITCH_NOT_PREPARED: &str = "self VT switch can be submitted only after preparation";

#[cfg(any(not(feature = "kms-live"), test))]
pub(crate) const LIVE_BODY_UNAVAILABLE_REASON: &str = "kms-live-body-unavailable";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuildProfile {
    kms_live_feature: bool,
    release: bool,
}

impl BuildProfile {
    fn current() -> Self {
        Self::from_build_markers(
            cfg!(feature = "kms-live"),
            cfg!(cosmix_kms_live_release),
            env!("COSMIX_KMS_LIVE_CARGO_PROFILE"),
        )
    }

    fn from_build_markers(kms_live_feature: bool, release_cfg: bool, cargo_profile: &str) -> Self {
        Self {
            kms_live_feature,
            release: release_cfg && cargo_profile == "release",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VtState {
    observation_available: bool,
    tty_is_character_device: bool,
    tty_alias_rdev: u64,
    foreground_process_group: bool,
    tty_major: u32,
    tty_minor: u32,
    active_vt: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeviceIdentity {
    observation_available: bool,
    observed_for: PathBuf,
    canonical_path: Option<PathBuf>,
    node_is_character_device: bool,
    node_is_primary_drm: bool,
    node_rdev: u64,
    udev_rdev: Option<u64>,
    stable_device_path: Option<PathBuf>,
    connectors: BTreeSet<String>,
}

impl DeviceIdentity {
    fn unavailable_for(path: PathBuf) -> Self {
        Self {
            observation_available: false,
            observed_for: path,
            canonical_path: None,
            node_is_character_device: false,
            node_is_primary_drm: false,
            node_rdev: 0,
            udev_rdev: None,
            stable_device_path: None,
            connectors: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct KmsLiveRequest {
    device: PathBuf,
    connector: String,
    presentation_backend: PresentationBackend,
    scene_mode: LiveSceneMode,
    output_scale: OutputScale120,
    decoration: DecorationStartup,
    /// Whether the operator asked for the interactive takeover confirmation
    /// (`--kms-confirm`). Off by default: a live takeover proceeds unattended so
    /// an agent can drive it without a human at the glass. When on, comp prints
    /// a fresh nonce to the controlling tty and refuses unless it is typed back
    /// — the pre-existing freshness interlock against a *blind* tty input
    /// injector, opted into for a human who wants the guard rail.
    confirm: bool,
}

#[derive(Debug, PartialEq)]
struct KmsLiveDecision {
    request: KmsLiveRequest,
    canonical_device: PathBuf,
    vt: u16,
    stable_device_path: PathBuf,
    drm_device: u64,
}

trait GrantPlatform {
    fn observe_vt(&self, tty: BorrowedFd<'_>) -> VtState;
    #[cfg(any(feature = "kms-live", test))]
    fn observe_device(&self, request: &KmsLiveRequest) -> DeviceIdentity;
    #[cfg(any(feature = "kms-live", test))]
    fn legacy_tiocsti_enabled(&self) -> Result<bool, KmsLiveRefusal>;
    #[cfg(any(feature = "kms-live", test))]
    fn fill_confirmation_nonce(&self, nonce: &mut [u8]) -> Result<(), KmsLiveRefusal>;
    #[cfg(any(feature = "kms-live", test))]
    fn hold_device_incarnation(
        &self,
        device: &DeviceIdentity,
    ) -> Result<DeviceIncarnationWitness, KmsLiveRefusal>;
    #[cfg(any(feature = "kms-live", test))]
    fn validate_device_incarnation(
        &self,
        witness: &DeviceIncarnationWitness,
        opened: &OpenDrmIdentity,
    ) -> Result<(), KmsLiveRefusal>;
    #[cfg(any(feature = "kms-live", test))]
    fn observe_open_drm(&self, fd: BorrowedFd<'_>) -> Result<OpenDrmIdentity, KmsLiveRefusal>;
    #[cfg(any(feature = "kms-live", test))]
    fn scan_connector(
        &self,
        fd: BorrowedFd<'_>,
        opened: &OpenDrmIdentity,
        connector: &str,
    ) -> Result<Option<ConnectorBinding>, KmsLiveRefusal>;
}

#[cfg(any(feature = "kms-live", test))]
trait ConfirmationIo {
    fn flush_input(&mut self, tty: BorrowedFd<'_>) -> Result<(), KmsLiveRefusal>;
    fn display_prompt(
        &mut self,
        tty: BorrowedFd<'_>,
        intent: &str,
        expected_code: &str,
    ) -> Result<(), KmsLiveRefusal>;
    fn read_line(&mut self, tty: BorrowedFd<'_>) -> Result<String, KmsLiveRefusal>;
}

#[cfg(any(feature = "kms-live", test))]
trait TtyKernelCalls {
    fn tcflush(&self, fd: libc::c_int, selector: libc::c_int) -> libc::c_int;
    fn tcgetpgrp(&self, fd: libc::c_int) -> libc::pid_t;
    fn getpgrp(&self) -> libc::pid_t;
    fn tiocgdev(&self, fd: libc::c_int, request: libc::c_ulong, output: &mut u32) -> libc::c_int;
    fn vt_getstate(
        &self,
        fd: libc::c_int,
        request: libc::c_ulong,
        output: &mut LinuxVtStat,
    ) -> libc::c_int;
}

#[cfg(any(feature = "kms-live", test))]
fn require_input_flush(result: libc::c_int) -> Result<(), KmsLiveRefusal> {
    if result != 0 {
        return Err(KmsLiveRefusal::TtyInputFlushFailed);
    }
    Ok(())
}

/// A live, non-transferable proof that the interlock passed once.
///
/// Holding this grant authorises nothing by itself. Its fields are private and
/// the only crate-visible consumer is [`execute_live`], which owns the final
/// VT, DRM identity and connector checks.
pub(crate) struct KmsLiveGrant {
    tty: OwnedFd,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    canonical_device: PathBuf,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    connector: String,
    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    presentation_backend: PresentationBackend,
    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    scene_mode: LiveSceneMode,
    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    output_scale: OutputScale120,
    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    decoration: DecorationStartup,
    authorised_vt: u16,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    stable_device_path: PathBuf,
    #[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
    drm_device: u64,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    incarnation: DeviceIncarnationWitness,
    platform: Rc<dyn GrantPlatform>,
}

struct DeviceIncarnationWitness {
    #[cfg(any(feature = "kms-live", test))]
    dev_attribute: OwnedFd,
    #[cfg(any(feature = "kms-live", test))]
    card_inode: u64,
    #[cfg(any(feature = "kms-live", test))]
    expected_rdev: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
struct OpenDrmIdentity {
    rdev: u64,
    stable_device_path: PathBuf,
    sysfs_card_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
struct ConnectorBinding {
    connector_id: u32,
}

#[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
struct VerifiedDrmFd {
    fd: Option<OwnedFd>,
    connector_id: u32,
    stable_device_path: PathBuf,
    device_path: PathBuf,
    device_id: u64,
    connector_name: String,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct PreparedLiveOperation {
    session: Option<SessionDeviceClient>,
    pump: Option<super::render::LiveRenderPump>,
    output_selector: Option<super::render::PreparedLiveOutputSelector>,
    protocol_wiring: Option<crate::protocol::WaylandGpuWiring>,
    signals: Option<LiveSignalWatcher>,
    pending_vt_switch: Option<u8>,
    pending_external_pause_ack: Option<ExternalPauseAcknowledgement>,
    initial_render_commands: Vec<KmsRenderCommand>,
    topology_client: Option<crate::protocol::KmsTopologyClient>,
    frame_clock: Option<crate::protocol::ClientFrameClock>,
    security_reporter: Option<crate::protocol::SecurityPresentationReporter>,
    scene_feed: Option<crate::protocol::ClientSceneFeed>,
    scene_mode: LiveSceneMode,
    decoration: DecorationStartup,
    #[cfg(feature = "bus")]
    bus_service: String,
    output_scale: OutputScale120,
    selected_output: Option<super::kms::SelectedOutput>,
    resume_mode: Option<super::kms::ConnectorMode>,
    lifecycle: Option<LiveCoordinatorLifecycle>,
    active_fd_baseline: LiveActiveFdBaseline,
    resume_cycle: u64,
    target_pairing: LiveTargetPairingLedger,
    last_active_scanout: Option<ResumeScanoutSnapshot>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveSelectedTarget {
    topology: super::kms::KmsTopologySnapshot,
    bootstrap_extent: (u32, u32),
}

#[cfg(any(not(feature = "kms-live"), test))]
struct PreparedLiveOperation;

#[cfg(all(feature = "kms-live", not(test)))]
struct SessionDeviceOwner {
    session: LibSeatSession,
    original: Option<OwnedFd>,
}

#[cfg(all(feature = "kms-live", not(test)))]
static_assertions::assert_not_impl_any!(SessionDeviceOwner: Clone, Copy, Send);

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveSessionState {
    target_device: u64,
    authority: LiveSessionAuthority,
    owner: Option<SessionDeviceOwner>,
    pending_open: Option<PendingLiveOpen>,
    pending_input_open: Option<PendingInputOpen>,
    revocations: LiveCoordinatorSender,
    /// The shutdown acknowledgement, held back until the teardown it
    /// acknowledges has actually happened.
    ///
    /// The shutdown handler cannot send it. Dropping `owner` there drops a
    /// `LibSeatSession`, which holds only a `Weak`; the strong `Rc` is inside
    /// the `LibSeatSessionNotifier` this thread's event loop owns as a source,
    /// so the foreign `libseat_close_seat` does not run until that event loop
    /// is destroyed — after the handler, after the dispatch, after the loop.
    /// Answering from the handler would tell the coordinator the teardown had
    /// finished while its only foreign call had not started, and the
    /// coordinator would then `join` a thread that can still block.
    shutdown_ack: Option<HeldShutdownAck>,
    stop: bool,
}

/// A shutdown answer, and the channel it will be sent on once the teardown it
/// answers for has finished. See [`LiveSessionState::shutdown_ack`].
#[cfg(all(feature = "kms-live", not(test)))]
struct HeldShutdownAck {
    reply: SyncSender<Result<(), String>>,
    result: Result<(), String>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct SessionDeviceClient {
    commands: channel::Sender<LiveSessionCommand>,
    events: Receiver<LiveCoordinatorEvent>,
    /// The sending end of `revocations`, kept so the input transport can be
    /// handed a clone of it.
    ///
    /// The session thread has its own clone and uses it for pause and hotplug.
    /// This one exists because the *protocol* thread also needs to be able to
    /// end the live operation — when the session thread stops answering, it is
    /// the only thread left that can say so.
    fatal: LiveCoordinatorSender,
    /// The seat libseat actually gave this session.
    ///
    /// Not `XDG_SEAT` and not the string `seat0`: an environment variable is a
    /// hint about what the launcher intended, while this is what the session
    /// manager decided. Assigning libinput a different seat from the one
    /// holding the devices produces a compositor with no input and no error.
    seat: String,
    /// Revocations observed by the non-blocking pre-adapter check.
    ///
    /// They stay here rather than being sent back through the channel: sending
    /// them back can reorder them against concurrent producers. The blocking
    /// wait consumes this prefix first, while an early setup failure carries it
    /// into `close` so teardown can re-decide against the same evidence.
    deferred_events: Vec<LiveCoordinatorEvent>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
enum LiveSessionCommand {
    Open {
        path: PathBuf,
        reply: SyncSender<Result<OwnedFd, KmsLiveError>>,
    },
    Duplicate {
        reply: SyncSender<Result<OwnedFd, String>>,
    },
    CaptureScanout {
        connector_id: u32,
        connector_identity: String,
        lifecycle_generation: u64,
        observed_at: Duration,
        old_output_target_existed: bool,
        expected_primary_plane_id: Option<u32>,
        reply: SyncSender<Result<ResumeScanoutSnapshot, String>>,
    },
    SwitchVt {
        vt: u8,
        confirm_self_pause: bool,
        reply: SyncSender<Result<(), String>>,
    },
    BeginSelfSwitch {
        generation: u64,
        reply: SyncSender<Result<(), KmsLiveError>>,
    },
    CloseOriginal {
        reply: SyncSender<Result<(), String>>,
    },
    BeginResume {
        reply: SyncSender<Result<u64, String>>,
    },
    ReturnPaused {
        generation: u64,
        cause: LivePauseCause,
        reply: SyncSender<Result<(), String>>,
    },
    FinishResume {
        generation: u64,
        reply: SyncSender<Result<(), String>>,
    },
    Shutdown {
        reply: SyncSender<Result<(), String>>,
    },
    /// Open one `/dev/input/event*` node on libinput's behalf.
    ///
    /// Separate from `Open` rather than sharing it, because `Open` is the DRM
    /// authority transition and is one-shot: it refuses once the authority is
    /// `Open`. Routed through it, the first input device would either consume
    /// the slot the DRM node needs or — after DRM is up, which is always —
    /// be refused outright.
    OpenInput {
        path: PathBuf,
        flags: i32,
        reply: SyncSender<Result<OwnedFd, i32>>,
    },
    /// Give a descriptor back so libseat can close the device behind it.
    ///
    /// The acknowledgement is bounded at the protocol end. Device removal and
    /// libinput teardown may pay one session round trip; ordinary input events
    /// never take this path. Without the answer a synchronous libseat close that
    /// wedged would leave the coordinator waiting forever with no later open
    /// required to expose it. The command owns the descriptor from the moment it
    /// is constructed, so if the channel is gone the descriptor comes back in
    /// the `SendError` and is dropped there.
    CloseInput {
        fd: OwnedFd,
        reply: SyncSender<Result<(), String>>,
    },
}

#[cfg(all(feature = "kms-live", not(test)))]
struct PendingLiveOpen {
    path: PathBuf,
    reply: SyncSender<Result<OwnedFd, KmsLiveError>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
struct PendingInputOpen {
    path: PathBuf,
    flags: i32,
    reply: SyncSender<Result<OwnedFd, i32>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) enum LiveRevocation {
    SessionPause,
    TargetHotplug,
    /// The session thread stopped answering an operation within its deadline.
    ///
    /// Published by a timed-out input-device open or close and by the bounded
    /// startup waits alike, so it names no single operation — the log at the
    /// publishing site does. Unlike the other two this is not the session
    /// manager taking the device
    /// away; it is this compositor failing. It travels the same channel because
    /// the coordinator is already blocked on that channel and the outcome is the
    /// same — end the live operation — but the *teardown* differs, because the
    /// thread that must be asked to shut down is the thread that stopped
    /// answering. See [`session_teardown_after`].
    SessionUnresponsive,
    /// The session thread is leaving, for any reason at all.
    ///
    /// Announced by the thread itself on the way out — including out of a panic
    /// — rather than inferred from the channel disconnecting. Inference used to
    /// be enough: the thread held the only `Sender`, so its exit produced a
    /// `RecvError`. It is not enough any more, because the protocol thread's
    /// input transport holds a clone for [`SessionUnresponsive`](Self::SessionUnresponsive),
    /// and one live `Sender` anywhere keeps `recv` blocked forever. A coordinator
    /// that never wakes is exactly the failure this rung exists to remove, so the
    /// exit is stated rather than deduced.
    SessionThreadStopped,
    /// The Wayland protocol thread stopped because dispatch failed or panicked.
    ///
    /// The libseat session may still be healthy, so this is an internal
    /// compositor failure requiring graceful session teardown rather than a
    /// reason to leak the session thread.
    ProtocolThreadStopped,
}

#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct ExternalPauseAcknowledgement {
    acknowledge: Option<Box<dyn FnOnce() -> bool + Send>>,
}

#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
impl ExternalPauseAcknowledgement {
    fn new(acknowledge: impl FnOnce() -> bool + Send + 'static) -> Self {
        Self {
            acknowledge: Some(Box::new(acknowledge)),
        }
    }

    fn acknowledge(mut self) -> bool {
        self.acknowledge
            .take()
            .is_some_and(|acknowledge| acknowledge())
    }
}

impl fmt::Debug for ExternalPauseAcknowledgement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalPauseAcknowledgement")
            .field("pending", &self.acknowledge.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) enum LiveSignal {
    Interrupt,
    Terminate,
    Hangup,
}

#[cfg(all(feature = "kms-live", not(test)))]
static LIVE_SIGNAL_LATCH: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

impl LiveSignal {
    fn number(self) -> i32 {
        match self {
            Self::Interrupt => libc::SIGINT,
            Self::Terminate => libc::SIGTERM,
            Self::Hangup => libc::SIGHUP,
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn from_number(number: i32) -> Option<Self> {
        match number {
            libc::SIGINT => Some(Self::Interrupt),
            libc::SIGTERM => Some(Self::Terminate),
            libc::SIGHUP => Some(Self::Hangup),
            _ => None,
        }
    }

    fn exit_code(self) -> u8 {
        (128 + self.number()).try_into().unwrap_or(u8::MAX)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveSignalDeliveryAction {
    Latched,
    HardExit(i32),
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_signal_delivery_action(
    latch: &std::sync::atomic::AtomicI32,
    signal: LiveSignal,
) -> LiveSignalDeliveryAction {
    match latch.compare_exchange(
        0,
        signal.number(),
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    ) {
        Ok(_) => LiveSignalDeliveryAction::Latched,
        Err(first_signal) => LiveSignalDeliveryAction::HardExit(first_signal.saturating_add(128)),
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn latched_live_signal() -> Option<LiveSignal> {
    LiveSignal::from_number(LIVE_SIGNAL_LATCH.load(std::sync::atomic::Ordering::Acquire))
}

#[cfg(any(not(feature = "kms-live"), test))]
fn latched_live_signal() -> Option<LiveSignal> {
    None
}

pub(crate) fn latched_signal_exit_code() -> Option<u8> {
    latched_live_signal().map(LiveSignal::exit_code)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(test, allow(dead_code))]
#[derive(Debug)]
pub(crate) enum LiveCoordinatorEvent {
    Revocation(LiveRevocation),
    Pump(super::render::PumpReply),
    Signal(LiveSignal),
    VtSwitchRequested(u8),
    PauseRequested {
        generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
    },
    SessionPaused {
        generation: u64,
        resumable: bool,
    },
    SessionPauseConfirmed {
        generation: u64,
    },
    SessionActivate {
        generation: u64,
    },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn classify_pre_supervision_terminal(
    latched_signal: Option<LiveSignal>,
    event: Option<LiveCoordinatorEvent>,
    phase: &'static str,
) -> Result<Option<LiveSupervisionEnd>, KmsLiveError> {
    if let Some(signal) = latched_signal {
        return Ok(Some(LiveSupervisionEnd::Signal(signal)));
    }
    match event {
        None => Ok(None),
        Some(LiveCoordinatorEvent::Signal(signal)) => Ok(Some(LiveSupervisionEnd::Signal(signal))),
        Some(LiveCoordinatorEvent::Revocation(
            revocation @ (LiveRevocation::SessionPause | LiveRevocation::TargetHotplug),
        )) => Ok(Some(LiveSupervisionEnd::Revocation(revocation))),
        Some(LiveCoordinatorEvent::Revocation(revocation)) => Err(KmsLiveError::Setup(format!(
            "live session ended {phase}: {revocation:?}"
        ))),
        Some(LiveCoordinatorEvent::Pump(reply)) => Err(KmsLiveError::Setup(format!(
            "live render pump sent an unexpected reply {phase}: {reply:?}"
        ))),
        Some(LiveCoordinatorEvent::VtSwitchRequested(vt)) => {
            Ok(Some(LiveSupervisionEnd::VtSwitchRequested {
                vt,
                outstanding_command: None,
            }))
        }
        Some(LiveCoordinatorEvent::PauseRequested {
            generation,
            acknowledgement,
        }) => Ok(Some(LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command: None,
        })),
        Some(
            LiveCoordinatorEvent::SessionPaused { .. }
            | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
            | LiveCoordinatorEvent::SessionActivate { .. },
        ) => Err(KmsLiveError::Setup(format!(
            "live session sent an unexpected lifecycle confirmation {phase}"
        ))),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone)]
struct LiveCoordinatorSender(mpsc::Sender<LiveCoordinatorEvent>);

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveCoordinatorSender {
    fn send_revocation(
        &self,
        revocation: LiveRevocation,
    ) -> Result<(), mpsc::SendError<LiveCoordinatorEvent>> {
        self.0.send(LiveCoordinatorEvent::Revocation(revocation))
    }

    fn sender(&self) -> mpsc::Sender<LiveCoordinatorEvent> {
        self.0.clone()
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LiveRevocationPublisher {
    fn publish(&self, revocation: LiveRevocation);
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRevocationPublisher for LiveCoordinatorSender {
    fn publish(&self, revocation: LiveRevocation) {
        let _ = self.send_revocation(revocation);
    }
}

#[cfg(test)]
impl LiveRevocationPublisher for mpsc::Sender<LiveRevocation> {
    fn publish(&self, revocation: LiveRevocation) {
        let _ = self.send(revocation);
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LiveSignalWatcher {
    handle: signal_hook::iterator::Handle,
    thread: Option<JoinHandle<()>>,
}

#[cfg(all(feature = "kms-live", not(test)))]
fn handle_live_signal_delivery(signal: LiveSignal) {
    if let LiveSignalDeliveryAction::HardExit(code) =
        live_signal_delivery_action(&LIVE_SIGNAL_LATCH, signal)
    {
        // POSIX specifies `_exit` as async-signal-safe. This handler performs
        // only atomic operations, local integer work and this terminal syscall;
        // it must never allocate, lock, trace or run process teardown.
        unsafe { libc::_exit(code) };
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn install_live_signal_delivery_handlers() -> Result<(), KmsLiveError> {
    let mut registrations = Vec::with_capacity(3);
    for signal in [
        LiveSignal::Interrupt,
        LiveSignal::Terminate,
        LiveSignal::Hangup,
    ] {
        // SAFETY: the registered closure calls `handle_live_signal_delivery`,
        // whose only shared access is an AtomicI32 and whose sole OS call is
        // async-signal-safe `_exit` on a subsequent delivery.
        match unsafe {
            signal_hook::low_level::register(signal.number(), move || {
                handle_live_signal_delivery(signal);
            })
        } {
            Ok(registration) => registrations.push(registration),
            Err(error) => {
                for registration in registrations {
                    signal_hook::low_level::unregister(registration);
                }
                return Err(KmsLiveError::Setup(format!(
                    "signal handler registration failed: {error}"
                )));
            }
        }
    }
    // SigId is only an explicit unregister token. Letting these tokens go does
    // not unregister the actions: the atomic latch and second-signal `_exit`
    // remain process-lifetime guards after the iterator watcher is dropped.
    Ok(())
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveSignalWatcher {
    fn start(events: mpsc::Sender<LiveCoordinatorEvent>) -> Result<Self, KmsLiveError> {
        // signal-hook runs actions in registration order. Install the atomic
        // action before the iterator's pipe-wakeup action so a woken watcher
        // can only observe an already-latched first signal.
        install_live_signal_delivery_handlers()?;
        let mut signals =
            signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM, libc::SIGHUP])
                .map_err(|error| KmsLiveError::Setup(format!("signal watcher failed: {error}")))?;
        let handle = signals.handle();
        let thread = thread::Builder::new()
            .name("cosmix-kms-signal".into())
            .spawn(move || {
                for _raw in signals.forever() {
                    let Some(signal) = latched_live_signal() else {
                        continue;
                    };
                    if events.send(LiveCoordinatorEvent::Signal(signal)).is_err() {
                        return;
                    }
                    return;
                }
            })
            .map_err(|error| {
                KmsLiveError::Setup(format!("signal watcher thread failed: {error}"))
            })?;
        Ok(Self {
            handle,
            thread: Some(thread),
        })
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for LiveSignalWatcher {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Announce the session thread's exit to the coordinator, whatever causes it.
///
/// A guard rather than a `send` at each return point, because the paths out of
/// that thread include an unwinding panic, and only a destructor covers that.
#[cfg(all(feature = "kms-live", not(test)))]
struct SessionThreadExitGuard(LiveCoordinatorSender);

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for SessionThreadExitGuard {
    fn drop(&mut self) {
        // Unbounded, so this cannot block a thread that is already unwinding.
        // On an ordinary shutdown the coordinator has long since returned from
        // its wait and nobody reads this; the value is dropped with the
        // receiver, which costs nothing.
        let _ = self.0.send_revocation(LiveRevocation::SessionThreadStopped);
    }
}

/// How to end the session thread, given what ended the live operation.
///
/// A separate decision from "should we stop", because the two ordinary
/// revocations leave a healthy session thread that will answer a shutdown
/// command, while an unresponsive one by definition will not, and asking a
/// wedged thread to shut down would spend the shutdown deadline discovering
/// again what is already known.
#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionTeardown {
    /// Send `Shutdown` and wait for the acknowledgement within
    /// `SESSION_SHUTDOWN_TIMEOUT`; join the thread only if it arrives.
    ///
    /// Not a promise that the thread is healthy — only that nothing yet says
    /// otherwise. Every way the ask can end without an acknowledgement falls
    /// back to detaching, because a thread can wedge after this was chosen and
    /// silence does not distinguish a wedge from an exit.
    Graceful,
    /// Abandon the thread without asking it anything and without joining.
    ///
    /// The process is ending; an unjoined thread is a leak that lasts until
    /// exit, which is strictly better than a teardown that never returns.
    Detach,
}

/// Whether the render adapter may start after protocol startup examined every
/// queued coordinator event and the durable signal latch.
#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdapterStartDecision {
    Start,
    EndAuthority(LiveRevocation),
    EndSignal(LiveSignal),
    EndVtSwitch(u8),
    RefuseInternal(LiveRevocation),
}

/// Decide adapter admission from the bounded prefix the non-blocking startup
/// drain observed.
///
/// Any revocation refuses, not only an internal fatal: once authority has been
/// paused or the target changed, starting a renderer with the already-invalid
/// lease would be just as wrong as starting one after an input timeout. The
/// values remain owned by the session client so the teardown funnel sees the
/// same evidence and takes the final decision.
///
/// This closes the already-queued case only — it does not make admission
/// race-free. A revocation published after the drain and before the blocking
/// wait is not observed until adapter startup finishes, and both ways that
/// can finish are bounded. Startup that fails — including DRM work the kernel
/// refuses once authority is gone, bounded by the render worker's reply
/// deadline (`backend/render.rs`) — propagates through `start_adapter`'s
/// error into the teardown funnel, where the drain in `close` re-reads the
/// channel and sees what this check missed. Startup that succeeds — possible
/// even mid-revocation, and the ordinary case for a revocation that touches
/// no DRM authority at all, such as a stopped protocol thread — proceeds
/// straight to the blocking wait, which consumes the queued value
/// immediately.
#[cfg(any(feature = "kms-live", test))]
fn adapter_start_after_revocations(revocations: &[LiveRevocation]) -> AdapterStartDecision {
    if let Some(failure) = revocations.iter().copied().find(|revocation| {
        matches!(
            revocation,
            LiveRevocation::SessionUnresponsive
                | LiveRevocation::SessionThreadStopped
                | LiveRevocation::ProtocolThreadStopped
        )
    }) {
        return AdapterStartDecision::RefuseInternal(failure);
    }
    revocations
        .first()
        .copied()
        .map(AdapterStartDecision::EndAuthority)
        .unwrap_or(AdapterStartDecision::Start)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn adapter_start_after_events(
    events: &[LiveCoordinatorEvent],
    latched_signal: Option<LiveSignal>,
) -> AdapterStartDecision {
    if let Some(signal) = latched_signal.or_else(|| {
        events.iter().find_map(|event| match event {
            LiveCoordinatorEvent::Signal(signal) => Some(*signal),
            LiveCoordinatorEvent::Revocation(_)
            | LiveCoordinatorEvent::Pump(_)
            | LiveCoordinatorEvent::VtSwitchRequested(_)
            | LiveCoordinatorEvent::PauseRequested { .. }
            | LiveCoordinatorEvent::SessionPaused { .. }
            | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
            | LiveCoordinatorEvent::SessionActivate { .. } => None,
        })
    }) {
        return AdapterStartDecision::EndSignal(signal);
    }
    let first_vt = events
        .iter()
        .position(|event| matches!(event, LiveCoordinatorEvent::VtSwitchRequested(_)));
    let first_revocation = events
        .iter()
        .position(|event| matches!(event, LiveCoordinatorEvent::Revocation(_)));
    if first_vt.is_some_and(|vt| first_revocation.is_none_or(|revocation| vt < revocation)) {
        let LiveCoordinatorEvent::VtSwitchRequested(vt) = &events[first_vt.expect("checked above")]
        else {
            unreachable!("the recorded position names a VT-switch request")
        };
        return AdapterStartDecision::EndVtSwitch(*vt);
    }
    adapter_start_after_revocations(&queued_revocations(events))
}

#[cfg(any(feature = "kms-live", test))]
fn session_teardown_after(revocation: Option<LiveRevocation>) -> SessionTeardown {
    match revocation {
        Some(LiveRevocation::SessionUnresponsive) => SessionTeardown::Detach,
        // `None` is the ordinary path where nothing was revoked at all — a
        // clean shutdown — and it must stay graceful, since detaching there
        // would skip the session close on every normal exit.
        // A thread that has already left answers `join` immediately and fails
        // the shutdown send outright, so the ordinary route is bounded here and
        // reports the failed send rather than swallowing it.
        Some(
            LiveRevocation::SessionPause
            | LiveRevocation::TargetHotplug
            | LiveRevocation::SessionThreadStopped
            | LiveRevocation::ProtocolThreadStopped,
        )
        | None => SessionTeardown::Graceful,
    }
}

/// Re-decide the teardown against everything else the channel already holds.
///
/// The coordinator wakes on the *first* revocation and reads the channel exactly
/// once, so an ordinary pause published in the same dispatch round as a stalled
/// input open commits the teardown to `Graceful` and leaves the fatal
/// notification sitting unread behind it. `Detach` is the safer of the two
/// answers — it is the one that waits on nothing — so an unresponsive session
/// still queued wins over a graceful choice made a moment earlier, whatever the
/// order they arrived in.
///
/// This sharpens the answer; it is not what makes teardown safe. The thread can
/// wedge after the last queued message is drained, which no arbitration can see,
/// and that is why [`SessionDeviceClient::shutdown`] is bounded as well.
#[cfg(any(feature = "kms-live", test))]
fn teardown_upgraded_by(
    chosen: SessionTeardown,
    queued: impl IntoIterator<Item = LiveRevocation>,
) -> SessionTeardown {
    queued.into_iter().fold(chosen, |chosen, revocation| {
        match (chosen, session_teardown_after(Some(revocation))) {
            (SessionTeardown::Detach, _) | (_, SessionTeardown::Detach) => SessionTeardown::Detach,
            (SessionTeardown::Graceful, SessionTeardown::Graceful) => SessionTeardown::Graceful,
        }
    })
}

/// Re-decide a chosen teardown against everything still queued, and fold the
/// failures that decision carries into one result for the process exit.
///
/// One function on purpose: production [`SessionDeviceClient::close`] and the
/// orchestration fake both call it, so the composition — queued protocol
/// failure, teardown upgrade, upgraded-detach failure — cannot drift between
/// them. The drain, logging, shutdown and detach stay with their owners; the
/// decision does not.
#[cfg(any(feature = "kms-live", test))]
fn resolve_session_close(
    chosen: SessionTeardown,
    queued: &[LiveRevocation],
) -> (SessionTeardown, Result<(), KmsLiveError>) {
    let upgraded = teardown_upgraded_by(chosen, queued.iter().copied());
    let failure = combine_live_results(
        queued_protocol_failure_is_not_success(queued),
        upgraded_detach_is_not_success(chosen, upgraded),
    );
    (upgraded, failure)
}

/// A protocol failure found only during teardown must still reach the process
/// result even though it does not change how the healthy session thread closes.
#[cfg(any(feature = "kms-live", test))]
fn queued_protocol_failure_is_not_success(queued: &[LiveRevocation]) -> Result<(), KmsLiveError> {
    if queued.contains(&LiveRevocation::ProtocolThreadStopped) {
        Err(KmsLiveError::Setup(
            "the Wayland protocol thread stopped before the live operation ended".into(),
        ))
    } else {
        Ok(())
    }
}

/// Whether the session thread may still be waited on after a shutdown attempt.
#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionExit {
    /// It acknowledged the shutdown. `join` returns.
    Joinable,
    /// It did not acknowledge the shutdown. `join` may never return.
    Wedged,
}

/// How the coordinator's request for a shutdown ended.
#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownAsk {
    /// The command could not be sent: the thread's command source is gone.
    Unsent,
    /// The thread acknowledged it.
    Acknowledged,
    /// The reply channel was dropped without an answer.
    Dropped,
    /// The deadline passed with no answer.
    TimedOut,
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Debug, Eq, PartialEq)]
enum VtSwitchAsk {
    Accepted,
    Refused(String),
    #[cfg_attr(test, allow(dead_code))]
    Unsent,
    Dropped,
    TimedOut,
}

#[cfg(any(feature = "kms-live", test))]
fn vt_switch_ask_after_wait(
    wait: Result<Result<(), String>, mpsc::RecvTimeoutError>,
) -> VtSwitchAsk {
    match wait {
        Ok(Ok(())) => VtSwitchAsk::Accepted,
        Ok(Err(error)) => VtSwitchAsk::Refused(error),
        Err(mpsc::RecvTimeoutError::Disconnected) => VtSwitchAsk::Dropped,
        Err(mpsc::RecvTimeoutError::Timeout) => VtSwitchAsk::TimedOut,
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn require_accepted_self_switch(vt: u8, outcome: VtSwitchAsk) -> Result<(), KmsLiveError> {
    match outcome {
        VtSwitchAsk::Accepted => Ok(()),
        VtSwitchAsk::Refused(reason) if reason == SELF_SWITCH_NOT_PREPARED => {
            Err(KmsLiveError::AuthorityLost(LiveRevocation::SessionPause))
        }
        outcome => Err(KmsLiveError::Setup(format!(
            "live VT switch {vt} was not accepted: {outcome:?}"
        ))),
    }
}

#[cfg(test)]
fn self_switch_was_not_submitted(outcome: &VtSwitchAsk) -> bool {
    matches!(outcome, VtSwitchAsk::Refused(reason) if reason == SELF_SWITCH_NOT_PREPARED)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn is_external_authority_loss(error: &KmsLiveError) -> bool {
    matches!(
        error,
        KmsLiveError::AuthorityLost(LiveRevocation::SessionPause | LiveRevocation::TargetHotplug)
            | KmsLiveError::ExternalPauseRequested { .. }
    )
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn chord_is_stale_after_transition_failure(error: &KmsLiveError) -> bool {
    is_external_authority_loss(error) || matches!(error, KmsLiveError::TerminalFrame(_))
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn defer_vt_switch_after_transition_failure(
    pending_vt_switch: &mut Option<u8>,
    vt: u8,
    error: &KmsLiveError,
) -> bool {
    if chord_is_stale_after_transition_failure(error) {
        false
    } else {
        *pending_vt_switch = Some(vt);
        true
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn missing_self_pause_confirmation(generation: u64) -> KmsLiveError {
    KmsLiveError::Setup(format!(
        "self VT switch generation {generation} received no matching session-pause confirmation within {}ms",
        SELF_SWITCH_PAUSE_TIMEOUT.as_millis()
    ))
}

/// Turn a bounded wait for the shutdown acknowledgement into the ask outcome
/// and the error the teardown reports.
#[cfg(any(feature = "kms-live", test))]
fn shutdown_ask_after_wait(
    wait: Result<Result<(), String>, mpsc::RecvTimeoutError>,
) -> (ShutdownAsk, Result<(), KmsLiveError>) {
    match wait {
        Ok(closed) => (
            ShutdownAsk::Acknowledged,
            closed.map_err(KmsLiveError::Setup),
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => (
            ShutdownAsk::Dropped,
            Err(KmsLiveError::Setup(
                "live session shutdown reply was lost".into(),
            )),
        ),
        Err(mpsc::RecvTimeoutError::Timeout) => (
            ShutdownAsk::TimedOut,
            Err(KmsLiveError::Setup(format!(
                "the live session thread did not answer a shutdown within {}s; abandoning it \
                 rather than waiting on it",
                SESSION_SHUTDOWN_TIMEOUT.as_secs()
            ))),
        ),
    }
}

/// Decide, from how the ask ended, whether the thread can be joined.
///
/// Only an acknowledgement permits it, and the reason is narrower than it
/// looks. Dropping the `LibSeatSession` inside the shutdown handler frees
/// nothing: that type holds a `Weak`, and the strong `Rc` lives in the
/// `LibSeatSessionNotifier` the session thread's event loop owns as a source.
/// The foreign `libseat_close_seat` therefore runs when the event loop is
/// destroyed — after the handler, after the dispatch, after the loop. So the
/// acknowledgement is published from *after* that destruction (see
/// `LiveSessionState::shutdown_ack`), and it is the only event on any channel
/// that carries the fact that no foreign call is left to block in.
///
/// Nothing else carries it. A dropped reply channel and a failed send both mean
/// the thread stopped talking, which is compatible with it being stuck inside
/// libseat — the same silence a wedge produces. Treating any of them as
/// permission to `join` would restore exactly the unbounded wait this bound
/// exists to remove, so all three detach instead.
#[cfg(any(feature = "kms-live", test))]
fn session_exit_after_shutdown(ask: ShutdownAsk) -> SessionExit {
    match ask {
        ShutdownAsk::Acknowledged => SessionExit::Joinable,
        ShutdownAsk::Unsent | ShutdownAsk::Dropped | ShutdownAsk::TimedOut => SessionExit::Wedged,
    }
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveSessionAuthority {
    Preparing {
        generation: u64,
    },
    Active {
        generation: u64,
    },
    Paused {
        generation: u64,
        revocation: LiveRevocation,
    },
    ExternalPausing {
        generation: u64,
        activate_pending: bool,
    },
    SelfSwitching {
        generation: u64,
    },
    SelfPaused {
        generation: u64,
        pause_confirmed: bool,
    },
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LivePauseCause {
    External,
    SelfSwitch,
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LivePauseRequestDisposition {
    External { generation: u64 },
    SelfSwitch { generation: u64 },
    Duplicate,
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LivePauseCompletion {
    generation: u64,
    cause: LivePauseCause,
    resumable: bool,
    activate_pending: bool,
}

#[cfg(any(feature = "kms-live", test))]
impl LiveSessionAuthority {
    fn initial() -> Self {
        Self::Preparing { generation: 1 }
    }

    #[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
    fn begin_resume(&mut self) -> Result<u64, KmsLiveError> {
        let generation = match *self {
            Self::Paused { generation, .. }
            | Self::SelfPaused {
                generation,
                pause_confirmed: true,
            } => generation,
            _ => {
                return Err(KmsLiveError::Setup(
                    "session authority can resume only from a confirmed pause".into(),
                ));
            }
        };
        let generation = generation
            .checked_add(1)
            .ok_or_else(|| KmsLiveError::Setup("session authority generation exhausted".into()))?;
        *self = Self::Preparing { generation };
        Ok(generation)
    }

    fn request_pause(&mut self) -> Result<LivePauseRequestDisposition, KmsLiveError> {
        let disposition = match *self {
            Self::Preparing { generation } => {
                *self = Self::ExternalPausing {
                    generation,
                    activate_pending: false,
                };
                LivePauseRequestDisposition::External { generation }
            }
            Self::Active { generation } => {
                let generation = generation.checked_add(1).ok_or_else(|| {
                    KmsLiveError::Setup("session authority generation exhausted".into())
                })?;
                *self = Self::ExternalPausing {
                    generation,
                    activate_pending: false,
                };
                LivePauseRequestDisposition::External { generation }
            }
            Self::SelfSwitching { generation } => {
                *self = Self::ExternalPausing {
                    generation,
                    activate_pending: false,
                };
                LivePauseRequestDisposition::External { generation }
            }
            Self::SelfPaused {
                generation,
                pause_confirmed: false,
            } => LivePauseRequestDisposition::SelfSwitch { generation },
            Self::ExternalPausing { .. }
            | Self::Paused { .. }
            | Self::SelfPaused {
                pause_confirmed: true,
                ..
            } => LivePauseRequestDisposition::Duplicate,
        };
        Ok(disposition)
    }

    fn complete_pause(&mut self, resumable: bool) -> Option<LivePauseCompletion> {
        match *self {
            Self::ExternalPausing {
                generation,
                activate_pending,
            } => {
                *self = Self::Paused {
                    generation,
                    revocation: LiveRevocation::SessionPause,
                };
                Some(LivePauseCompletion {
                    generation,
                    cause: LivePauseCause::External,
                    resumable,
                    activate_pending,
                })
            }
            Self::SelfPaused {
                generation,
                pause_confirmed: false,
            } => {
                *self = Self::SelfPaused {
                    generation,
                    pause_confirmed: true,
                };
                Some(LivePauseCompletion {
                    generation,
                    cause: LivePauseCause::SelfSwitch,
                    resumable,
                    activate_pending: false,
                })
            }
            Self::Preparing { .. }
            | Self::Active { .. }
            | Self::Paused { .. }
            | Self::SelfSwitching { .. }
            | Self::SelfPaused {
                pause_confirmed: true,
                ..
            } => None,
        }
    }

    fn activate(&mut self) -> Option<u64> {
        match self {
            Self::ExternalPausing {
                activate_pending, ..
            } => {
                *activate_pending = true;
                None
            }
            Self::Paused { generation, .. }
            | Self::SelfPaused {
                generation,
                pause_confirmed: true,
            } => Some(*generation),
            Self::Preparing { .. }
            | Self::Active { .. }
            | Self::SelfSwitching { .. }
            | Self::SelfPaused {
                pause_confirmed: false,
                ..
            } => None,
        }
    }

    fn begin_self_switch(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        match *self {
            Self::Paused { revocation, .. } => {
                return Err(KmsLiveError::AuthorityLost(revocation));
            }
            Self::ExternalPausing { .. } => {
                return Err(KmsLiveError::AuthorityLost(LiveRevocation::SessionPause));
            }
            _ => {}
        }
        let Self::Active {
            generation: current,
        } = *self
        else {
            return Err(KmsLiveError::Setup(
                "self VT switch requires active session authority".into(),
            ));
        };
        if current.checked_add(1) != Some(generation) {
            return Err(KmsLiveError::Setup(format!(
                "kms-live-stale-generation: self-switch generation {generation} does not follow {current}"
            )));
        }
        *self = Self::SelfSwitching { generation };
        Ok(())
    }

    fn submit_self_switch(&mut self) -> Result<(), KmsLiveError> {
        let Self::SelfSwitching { generation } = *self else {
            return Err(KmsLiveError::Setup(SELF_SWITCH_NOT_PREPARED.into()));
        };
        *self = Self::SelfPaused {
            generation,
            pause_confirmed: false,
        };
        Ok(())
    }

    #[cfg(test)]
    fn activated_self_pause(&self) -> Option<u64> {
        match *self {
            Self::SelfPaused {
                generation,
                pause_confirmed: true,
            } => Some(generation),
            _ => None,
        }
    }

    #[cfg(test)]
    fn confirm_self_pause(&mut self) -> Option<u64> {
        self.complete_pause(true).and_then(|completion| {
            (completion.cause == LivePauseCause::SelfSwitch).then_some(completion.generation)
        })
    }

    #[cfg(test)]
    fn return_to_self_paused(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        self.return_to_paused(generation, LivePauseCause::SelfSwitch)
    }

    fn return_to_paused(
        &mut self,
        generation: u64,
        cause: LivePauseCause,
    ) -> Result<(), KmsLiveError> {
        match *self {
            Self::Preparing {
                generation: current,
            }
            | Self::Active {
                generation: current,
            } if current.checked_sub(1) == Some(generation)
                || current.checked_add(2) == Some(generation) =>
            {
                *self = match cause {
                    LivePauseCause::External => Self::Paused {
                        generation,
                        revocation: LiveRevocation::SessionPause,
                    },
                    LivePauseCause::SelfSwitch => Self::SelfPaused {
                        generation,
                        pause_confirmed: true,
                    },
                };
                Ok(())
            }
            _ => Err(KmsLiveError::Setup(format!(
                "session authority cannot return to paused generation {generation}"
            ))),
        }
    }

    fn finish_resume(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        let Self::Active {
            generation: current,
        } = *self
        else {
            return Err(KmsLiveError::Setup(
                "session authority can finish resume only while active".into(),
            ));
        };
        if current.checked_add(1) != Some(generation) {
            return Err(KmsLiveError::Setup(format!(
                "kms-live-stale-generation: resumed output generation {generation} does not follow session generation {current}"
            )));
        }
        *self = Self::Active { generation };
        Ok(())
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn deferred_pause_is_resumable(
    authority: LiveSessionAuthority,
    outcome: DeferredDisableOutcome,
) -> bool {
    match authority {
        // A self-switch reaches `SelfPaused` only after the renderer, input and
        // retained DRM fd have already been suspended or closed. Its deferred
        // acknowledgement is therefore protocol bookkeeping, not the proof of
        // local quiescence used by an external pause. If that bounded waiter
        // expires, a successful seat.disable() still leaves a valid disabled
        // session which must accept a later Enable.
        LiveSessionAuthority::SelfPaused {
            pause_confirmed: false,
            ..
        } => outcome.disable_succeeded,
        _ => outcome.resumable(),
    }
}

#[cfg(any(feature = "kms-live", test))]
fn session_authority_devices_are_revoked(authority: LiveSessionAuthority) -> bool {
    matches!(
        authority,
        LiveSessionAuthority::ExternalPausing { .. }
            | LiveSessionAuthority::Paused {
                revocation: LiveRevocation::SessionPause,
                ..
            }
            | LiveSessionAuthority::SelfPaused { .. }
    )
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveCoordinatorLifecycleState {
    Active { generation: u64 },
    Pausing { generation: u64 },
    Paused { generation: u64 },
    Resuming { generation: u64 },
    Terminal,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveCoordinatorLifecycleEvent {
    BeginPause {
        generation: u64,
    },
    Suspended {
        generation: u64,
    },
    BeginResume {
        generation: u64,
    },
    ResumeFailed {
        generation: u64,
    },
    OutputReady {
        generation: u64,
        observed_at: Duration,
    },
    FrameSubmitted {
        generation: u64,
        observed_at: Duration,
    },
    RequestUpdate,
    Signal,
    Fatal,
    PumpDetached,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveCoordinatorLifecycleAction {
    BeginPause,
    Paused,
    BeginResume,
    Active,
    IssueUpdate,
    Hold,
    Terminal,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct LiveCoordinatorLifecycleError {
    code: &'static str,
    detail: String,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
struct LiveCoordinatorLifecycle {
    state: LiveCoordinatorLifecycleState,
    last_submitted_at: Option<Duration>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg_attr(all(feature = "kms-live", not(test)), allow(dead_code))]
impl LiveCoordinatorLifecycle {
    fn active(generation: u64, ready_at: Duration) -> Self {
        Self {
            state: LiveCoordinatorLifecycleState::Active { generation },
            last_submitted_at: Some(ready_at),
        }
    }

    fn active_presentation_generation(&self) -> Option<u64> {
        match self.state {
            LiveCoordinatorLifecycleState::Active { generation } => Some(generation),
            _ => None,
        }
    }

    fn apply(
        &mut self,
        event: LiveCoordinatorLifecycleEvent,
    ) -> Result<LiveCoordinatorLifecycleAction, LiveCoordinatorLifecycleError> {
        if matches!(
            event,
            LiveCoordinatorLifecycleEvent::Signal
                | LiveCoordinatorLifecycleEvent::Fatal
                | LiveCoordinatorLifecycleEvent::PumpDetached
        ) {
            self.state = LiveCoordinatorLifecycleState::Terminal;
            self.last_submitted_at = None;
            return Ok(LiveCoordinatorLifecycleAction::Terminal);
        }
        match (self.state, event) {
            (
                LiveCoordinatorLifecycleState::Active {
                    generation: current,
                },
                LiveCoordinatorLifecycleEvent::BeginPause { generation },
            ) if current.checked_add(1) == Some(generation) => {
                self.state = LiveCoordinatorLifecycleState::Pausing { generation };
                self.last_submitted_at = None;
                Ok(LiveCoordinatorLifecycleAction::BeginPause)
            }
            (
                LiveCoordinatorLifecycleState::Pausing {
                    generation: expected,
                },
                LiveCoordinatorLifecycleEvent::Suspended { generation },
            ) if generation == expected => {
                self.state = LiveCoordinatorLifecycleState::Paused { generation };
                Ok(LiveCoordinatorLifecycleAction::Paused)
            }
            (
                LiveCoordinatorLifecycleState::Paused {
                    generation: current,
                },
                LiveCoordinatorLifecycleEvent::BeginResume { generation },
            ) if current.checked_add(1) == Some(generation) => {
                self.state = LiveCoordinatorLifecycleState::Resuming { generation };
                Ok(LiveCoordinatorLifecycleAction::BeginResume)
            }
            (
                LiveCoordinatorLifecycleState::Resuming {
                    generation: expected,
                },
                LiveCoordinatorLifecycleEvent::ResumeFailed { generation },
            ) if generation == expected.saturating_sub(1)
                || expected.checked_add(2) == Some(generation) =>
            {
                // Before the render transition starts, compensation returns
                // to the preceding Suspend generation. Once it starts, this
                // rung owns exactly one connector: Resume, AddOutput, then the
                // compensating Suspend, so the only other valid boundary is
                // `expected + 2`.
                self.state = LiveCoordinatorLifecycleState::Paused { generation };
                self.last_submitted_at = None;
                Ok(LiveCoordinatorLifecycleAction::Paused)
            }
            (
                LiveCoordinatorLifecycleState::Resuming {
                    generation: expected,
                },
                LiveCoordinatorLifecycleEvent::OutputReady {
                    generation,
                    observed_at,
                },
            ) if expected.checked_add(1) == Some(generation) => {
                self.state = LiveCoordinatorLifecycleState::Active { generation };
                self.last_submitted_at = Some(observed_at);
                Ok(LiveCoordinatorLifecycleAction::Active)
            }
            (
                LiveCoordinatorLifecycleState::Active {
                    generation: expected,
                },
                LiveCoordinatorLifecycleEvent::FrameSubmitted {
                    generation,
                    observed_at,
                },
            ) if generation == expected => {
                self.last_submitted_at = Some(observed_at);
                Ok(LiveCoordinatorLifecycleAction::Active)
            }
            (
                LiveCoordinatorLifecycleState::Active { .. },
                LiveCoordinatorLifecycleEvent::RequestUpdate,
            ) => Ok(LiveCoordinatorLifecycleAction::IssueUpdate),
            (
                LiveCoordinatorLifecycleState::Pausing { .. }
                | LiveCoordinatorLifecycleState::Paused { .. }
                | LiveCoordinatorLifecycleState::Resuming { .. },
                LiveCoordinatorLifecycleEvent::RequestUpdate,
            ) => Ok(LiveCoordinatorLifecycleAction::Hold),
            (LiveCoordinatorLifecycleState::Terminal, _) => {
                self.last_submitted_at = None;
                Ok(LiveCoordinatorLifecycleAction::Terminal)
            }
            (state, event) => Err(LiveCoordinatorLifecycleError {
                code: "kms-live-stale-generation",
                detail: format!("event {event:?} is invalid in state {state:?}"),
            }),
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn cancel_active_presentation_for_pause(
    lifecycle: &LiveCoordinatorLifecycle,
    pause_path: &'static str,
    cancel: impl FnOnce(u64),
) -> Result<u64, KmsLiveError> {
    let generation = lifecycle.active_presentation_generation().ok_or_else(|| {
        KmsLiveError::Setup(format!(
            "kms-live-{pause_path}-not-active: cannot cancel presentation from {:?}",
            lifecycle.state
        ))
    })?;
    cancel(generation);
    Ok(generation)
}

/// The fd count captured at the first OutputReady boundary.
///
/// `output_ready_observed` is distinct from `fd_count`: a failed `/proc` read
/// must not let a later generation replace the first-ready baseline.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LiveActiveFdBaseline {
    output_ready_observed: bool,
    fd_count: Option<usize>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LiveFdTelemetry {
    fd_count: Option<usize>,
    fd_delta: Option<isize>,
    first_output_ready: bool,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveActiveFdBaseline {
    fn observe_output_ready(&mut self, fd_count: Option<usize>) -> LiveFdTelemetry {
        let first_output_ready = !self.output_ready_observed;
        if first_output_ready {
            self.output_ready_observed = true;
            self.fd_count = fd_count;
        }
        LiveFdTelemetry {
            fd_count,
            fd_delta: live_fd_delta(fd_count, self.fd_count),
            first_output_ready,
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn live_fd_delta(fd_count: Option<usize>, baseline: Option<usize>) -> Option<isize> {
    fd_count.zip(baseline).map(|(count, baseline)| {
        isize::try_from(count).unwrap_or(isize::MAX)
            - isize::try_from(baseline).unwrap_or(isize::MAX)
    })
}

/// Live evidence that scanout targets were explicitly released.
///
/// `/proc/self/fd` cannot see allocator and renderer ownership, so the render worker
/// records every successful target creation and explicit release against the
/// topology generation. The coordinator owns a clone and reports the pairing
/// at lifecycle boundaries without synchronising with the worker beyond this
/// short mutex.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Default)]
pub(crate) struct LiveTargetPairingLedger {
    counts: Arc<Mutex<BTreeMap<u64, LiveTargetPairingCounts>>>,
    retained_counts: Arc<Mutex<BTreeMap<u64, LiveRetainedBufferPairingCounts>>>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LiveTargetPairingCounts {
    pub(crate) created: usize,
    pub(crate) released: usize,
}

/// Retained scanout storage is deliberately not an active output target.
/// Keeping its accounting in a separate species preserves the invariant that
/// target created/released counts describe only complete live targets.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LiveRetainedBufferPairingCounts {
    pub(crate) created: usize,
    pub(crate) released: usize,
    pub(crate) pending_handoffs: usize,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveRetainedBufferPairingCounts {
    #[cfg_attr(test, allow(dead_code))]
    fn is_balanced(self) -> bool {
        self.created == self.released
    }

    #[cfg_attr(test, allow(dead_code))]
    fn outstanding(self) -> usize {
        self.created.saturating_sub(self.released)
    }

    fn pending_handoff(self) -> bool {
        self.pending_handoffs != 0
    }

    #[cfg_attr(test, allow(dead_code))]
    fn is_healthy_while_paused(self) -> bool {
        self.released <= self.created && self.pending_handoffs == 0
    }

    fn is_healthy_while_active(self) -> bool {
        self.released <= self.created && self.outstanding() == self.pending_handoffs
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveTargetPairingCounts {
    fn is_paired(self) -> bool {
        self.created != 0 && self.created == self.released
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl LiveTargetPairingLedger {
    pub(crate) fn record_created(&self, generation: u64) {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(generation)
            .or_default()
            .created += 1;
    }

    pub(crate) fn record_released(&self, generation: u64) {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(generation)
            .or_default()
            .released += 1;
    }

    pub(crate) fn record_retained_created(&self, generation: u64) {
        self.retained_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(generation)
            .or_default()
            .created += 1;
    }

    pub(crate) fn record_retained_handoff_started(&self, generation: u64) {
        let mut retained = self
            .retained_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retained.entry(generation).or_default().pending_handoffs += 1;
    }

    pub(crate) fn record_retained_released(&self, generation: u64, pending_handoff: bool) {
        let mut retained = self
            .retained_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counts = retained.entry(generation).or_default();
        counts.released += 1;
        if pending_handoff {
            debug_assert!(counts.pending_handoffs != 0);
            counts.pending_handoffs = counts.pending_handoffs.saturating_sub(1);
        }
    }

    fn snapshot(&self, generation: u64) -> LiveTargetPairingCounts {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&generation)
            .copied()
            .unwrap_or_default()
    }

    fn inactive_snapshot(&self, active_generation: u64) -> LiveTargetPairingCounts {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(generation, _)| **generation < active_generation)
            .fold(
                LiveTargetPairingCounts::default(),
                |mut total, (_, counts)| {
                    total.created += counts.created;
                    total.released += counts.released;
                    total
                },
            )
    }

    pub(crate) fn retained_snapshot(&self, generation: u64) -> LiveRetainedBufferPairingCounts {
        self.retained_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&generation)
            .copied()
            .unwrap_or_default()
    }

    #[cfg_attr(test, allow(dead_code))]
    fn inactive_retained_snapshot(
        &self,
        active_generation: u64,
    ) -> LiveRetainedBufferPairingCounts {
        self.retained_counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(generation, _)| **generation < active_generation)
            .fold(
                LiveRetainedBufferPairingCounts::default(),
                |mut total, (_, counts)| {
                    total.created += counts.created;
                    total.released += counts.released;
                    total.pending_handoffs += counts.pending_handoffs;
                    total
                },
            )
    }
}

#[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
pub(crate) struct MasterDrmLease {
    pub(crate) fd: OwnedFd,
}

#[derive(Debug)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum KmsLiveError {
    Refused(KmsLiveRefusal),
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    AuthorityLost(LiveRevocation),
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    TerminalFrame(String),
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    Setup(String),
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    PumpDetached(String),
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    ExternalPauseRequested {
        generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
    },
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    Signal(LiveSignal),
}

impl From<KmsLiveRefusal> for KmsLiveError {
    fn from(refusal: KmsLiveRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl PartialEq<KmsLiveRefusal> for KmsLiveError {
    fn eq(&self, other: &KmsLiveRefusal) -> bool {
        matches!(self, Self::Refused(refusal) if refusal == other)
    }
}

impl fmt::Display for KmsLiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::AuthorityLost(revocation) => {
                write!(formatter, "live authority was revoked: {revocation:?}")
            }
            Self::TerminalFrame(detail) => formatter.write_str(detail),
            Self::Setup(detail) => formatter.write_str(detail),
            Self::PumpDetached(detail) => formatter.write_str(detail),
            Self::ExternalPauseRequested { generation, .. } => write!(
                formatter,
                "external pause generation {generation} interrupted a render transition"
            ),
            Self::Signal(signal) => write!(formatter, "received signal {}", signal.number()),
        }
    }
}

impl KmsLiveError {
    pub(crate) fn reason_code(&self) -> &'static str {
        match self {
            Self::Refused(refusal) => refusal.reason_code(),
            Self::AuthorityLost(_) => "kms-live-authority-lost",
            Self::TerminalFrame(_) => "kms-live-terminal-frame",
            Self::Setup(_) => "kms-live-setup-failed",
            Self::PumpDetached(_) => "kms-live-pump-detached",
            Self::ExternalPauseRequested { .. } => "kms-live-external-pause-requested",
            Self::Signal(_) => "kms-live-signal",
        }
    }

    pub(crate) fn exit_code(&self) -> Option<u8> {
        preferred_live_exit_code(self, latched_live_signal())
    }
}

fn preferred_live_exit_code(error: &KmsLiveError, latched: Option<LiveSignal>) -> Option<u8> {
    latched.map(LiveSignal::exit_code).or_else(|| match error {
        KmsLiveError::Signal(signal) => Some(signal.exit_code()),
        KmsLiveError::Refused(_)
        | KmsLiveError::AuthorityLost(_)
        | KmsLiveError::TerminalFrame(_)
        | KmsLiveError::Setup(_)
        | KmsLiveError::PumpDetached(_)
        | KmsLiveError::ExternalPauseRequested { .. } => None,
    })
}

impl Error for KmsLiveError {}

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for SessionDeviceOwner {
    fn drop(&mut self) {
        if let Err(error) = self.close_original(false) {
            tracing::error!(%error, "failed to close libseat DRM device");
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl SessionDeviceOwner {
    fn close_original(&mut self, authority_already_revoked: bool) -> Result<(), String> {
        close_retained_session_device(&mut self.original, |fd| {
            close_libseat_device(&mut self.session, fd, authority_already_revoked)
        })
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn close_libseat_device(
    session: &mut LibSeatSession,
    fd: OwnedFd,
    authority_already_revoked: bool,
) -> Result<(), String> {
    match session.close(fd) {
        Ok(()) => Ok(()),
        Err(error)
            if already_revoked_device_close_is_complete(
                authority_already_revoked,
                error.as_errno(),
            ) =>
        {
            tracing::debug!(%error, "already-revoked libseat device was locally closed");
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(any(feature = "kms-live", test))]
fn already_revoked_device_close_is_complete(
    authority_already_revoked: bool,
    errno: Option<i32>,
) -> bool {
    authority_already_revoked && errno == Some(libc::ENODEV)
}

#[cfg(any(feature = "kms-live", test))]
fn close_retained_session_device<E>(
    original: &mut Option<OwnedFd>,
    close: impl FnOnce(OwnedFd) -> Result<(), E>,
) -> Result<(), E> {
    original.take().map(close).unwrap_or(Ok(()))
}

#[cfg(any(feature = "kms-live", test))]
/// Latch the first revocation and report whether its coordinator event is owed.
///
/// `Preparing` still transitions to `Paused` so the authority-open guard is
/// independent of mailbox delivery, but that transition owes the same wake as
/// an already-open session: preparation supervision must end immediately.
fn latch_live_revocation(authority: &mut LiveSessionAuthority, revocation: LiveRevocation) -> bool {
    match *authority {
        LiveSessionAuthority::Preparing { generation }
        | LiveSessionAuthority::Active { generation } => {
            *authority = LiveSessionAuthority::Paused {
                generation,
                revocation,
            };
            true
        }
        LiveSessionAuthority::Paused { .. }
        | LiveSessionAuthority::ExternalPausing { .. }
        | LiveSessionAuthority::SelfPaused { .. } => false,
        LiveSessionAuthority::SelfSwitching { generation } => {
            *authority = LiveSessionAuthority::Paused {
                generation,
                revocation,
            };
            true
        }
    }
}

#[cfg(any(feature = "kms-live", test))]
#[cfg_attr(test, allow(dead_code))]
fn publish_latched_live_revocation(
    authority: &mut LiveSessionAuthority,
    revocation: LiveRevocation,
    publisher: &impl LiveRevocationPublisher,
) {
    if latch_live_revocation(authority, revocation) {
        publisher.publish(revocation);
    }
}

#[cfg(any(feature = "kms-live", test))]
fn open_authorised_session_device<E>(
    authority: &mut LiveSessionAuthority,
    session_active: bool,
    original: &mut Option<OwnedFd>,
    open: impl FnOnce() -> Result<OwnedFd, E>,
) -> Result<OwnedFd, KmsLiveError>
where
    E: fmt::Display,
{
    match *authority {
        LiveSessionAuthority::Paused { .. }
        | LiveSessionAuthority::ExternalPausing { .. }
        | LiveSessionAuthority::SelfSwitching { .. }
        | LiveSessionAuthority::SelfPaused { .. } => {
            return Err(KmsLiveRefusal::RevokedBeforeAuthorityOpen.into());
        }
        LiveSessionAuthority::Preparing { .. } => {}
        LiveSessionAuthority::Active { .. } => {
            return Err(KmsLiveError::Setup(
                "libseat DRM device is already open".into(),
            ));
        }
    }
    if !session_active {
        return Err(KmsLiveRefusal::SessionInactiveBeforeAuthorityOpen.into());
    }
    if original.is_some() {
        return Err(KmsLiveError::Setup(
            "libseat DRM device is already retained".into(),
        ));
    }

    let fd = open().map_err(|error| {
        tracing::error!(%error, "libseat failed to open the authorised DRM device");
        KmsLiveError::from(KmsLiveRefusal::DrmNodeOpenFailed)
    })?;
    *original = Some(fd);
    let LiveSessionAuthority::Preparing { generation } = *authority else {
        unreachable!("authority state was checked above")
    };
    *authority = LiveSessionAuthority::Active { generation };
    original
        .as_ref()
        .expect("the libseat original was retained immediately above")
        .as_fd()
        .try_clone_to_owned()
        .map_err(|error| KmsLiveError::Setup(format!("DRM verification fd dup failed: {error}")))
}

#[cfg(any(feature = "kms-live", test))]
fn queue_live_session_open<T>(pending_open: &mut Option<T>, request: T) -> Result<(), T> {
    if pending_open.is_some() {
        return Err(request);
    }
    *pending_open = Some(request);
    Ok(())
}

#[cfg(any(feature = "kms-live", test))]
fn dispatch_live_session_round<S, E>(
    state: &mut S,
    dispatch: impl FnOnce(&mut S) -> Result<(), E>,
    is_stopped: impl FnOnce(&S) -> bool,
    perform_pending_open: impl FnOnce(&mut S),
) -> Result<(), E> {
    // A pending command carries no authority to open until the complete
    // readiness batch has had the chance to latch a revocation.
    dispatch(state)?;
    if !is_stopped(state) {
        perform_pending_open(state);
    }
    Ok(())
}

#[cfg(all(feature = "kms-live", not(test)))]
fn publish_live_revocation(state: &mut LiveSessionState, revocation: LiveRevocation) {
    publish_latched_live_revocation(&mut state.authority, revocation, &state.revocations);
}

#[cfg(all(feature = "kms-live", not(test)))]
fn perform_pending_session_open(state: &mut LiveSessionState) {
    let Some(PendingLiveOpen { path, reply }) = state.pending_open.take() else {
        return;
    };
    let owner = state
        .owner
        .as_mut()
        .expect("session owner exists until shutdown");
    let SessionDeviceOwner { session, original } = owner;
    let result =
        open_authorised_session_device(&mut state.authority, session.is_active(), original, || {
            session.open(&path, OFlags::RDWR | OFlags::CLOEXEC)
        });
    let _ = reply.send(result);
}

/// Perform the deferred input open, if one is queued.
///
/// Runs in the same post-dispatch step as the DRM open and for the same reason:
/// a command that arrived in this readiness batch carries no authority until the
/// whole batch has had its chance to latch a revocation. An input open issued
/// before the pause event in the same batch was seen would be an open into a
/// session this process no longer owns.
#[cfg(all(feature = "kms-live", not(test)))]
fn perform_pending_input_open(state: &mut LiveSessionState) {
    let Some(pending) = state.pending_input_open.take() else {
        return;
    };
    let path = pending.path.clone();
    let started = Instant::now();
    let outcome = perform_input_open(state, pending);
    tracing::debug!(
        path = %path.display(),
        elapsed_us = started.elapsed().as_micros(),
        outcome,
        "session thread finished servicing a libinput device open"
    );
}

#[cfg(all(feature = "kms-live", not(test)))]
fn perform_input_open(
    state: &mut LiveSessionState,
    PendingInputOpen { path, flags, reply }: PendingInputOpen,
) -> &'static str {
    let Some(SessionDeviceOwner { session, .. }) = state.owner.as_mut() else {
        let _ = reply.send(Err(InputOpenRefusal::SessionInactive.errno()));
        return "session-gone";
    };

    // `symlink_metadata`, not `stat`: resolving a symlink here and then
    // approving what it pointed at is precisely the widening the path predicate
    // exists to prevent.
    let observed = rustix::fs::lstat(&path).ok().map(|stat| {
        observe_node(
            stat.st_mode as rustix::fs::RawMode,
            stat.st_rdev,
            stat.st_dev,
            stat.st_ino,
        )
    });
    let authority_open = matches!(state.authority, LiveSessionAuthority::Active { .. });
    let revoked = matches!(
        state.authority,
        LiveSessionAuthority::Paused { .. }
            | LiveSessionAuthority::ExternalPausing { .. }
            | LiveSessionAuthority::SelfSwitching { .. }
            | LiveSessionAuthority::SelfPaused { .. }
    );
    if let Err(refusal) = authorise_input_open(
        authority_open,
        revoked,
        session.is_active(),
        &path,
        observed,
    ) {
        tracing::warn!(path = %path.display(), ?refusal, "refused a libinput device open");
        let _ = reply.send(Err(refusal.errno()));
        return "refused";
    }
    let authorised = observed.expect("authorisation above requires an observed node");

    let fd = match session.open(&path, input_open_flags(flags)) {
        Ok(fd) => fd,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "libseat failed to open an input device");
            let _ = reply.send(Err(InputOpenRefusal::NodeNotObservable.errno()));
            return "libseat-open-failed";
        }
    };

    // The path was inspected, then opened; between the two it could have been
    // re-pointed. Only the descriptor can settle which node was actually
    // opened, so the check is repeated through it.
    let opened = rustix::fs::fstat(&fd).ok().map(|stat| {
        observe_node(
            stat.st_mode as rustix::fs::RawMode,
            stat.st_rdev,
            stat.st_dev,
            stat.st_ino,
        )
    });
    if let Err(refusal) = verify_opened_input_node(authorised, opened) {
        tracing::error!(path = %path.display(), ?refusal, "the opened input node is not the one authorised");
        // Handed back through the session, never dropped — dropping closes the
        // kernel fd but strands the `libseat::Device` in the session's map.
        let _ = close_session_input_fd(state, fd);
        let _ = reply.send(Err(refusal.errno()));
        return "opened-node-rejected";
    }
    if let Err(error) = ensure_close_on_exec(fd.as_fd()) {
        tracing::error!(path = %path.display(), %error, "an input descriptor could not be made close-on-exec");
        let _ = close_session_input_fd(state, fd);
        let _ = reply.send(Err(error.raw_os_error()));
        return "close-on-exec-failed";
    }
    // Not `let _ = reply.send(..)`. A failed send means nobody received the
    // descriptor — the caller timed out and dropped its receiver — and it comes
    // back still owned. Dropping it here would close the kernel fd while
    // leaving the `libseat::Device` in the session's map, and the fd number
    // could then be recycled over that live entry. It goes back through libseat
    // like every other descriptor. `deliver_open_reply` is what makes that
    // unconditional, and it is tested: exactly one reclaim for a rejected `Ok`,
    // none for anything else.
    deliver_open_reply(&reply, Ok(fd), |fd| {
        tracing::warn!(
            path = %path.display(),
            "an input descriptor was opened after its caller stopped waiting; closing it"
        );
        let _ = close_session_input_fd(state, fd);
    });
    "opened"
}

/// Close one input descriptor through libseat.
///
/// Permitted after revocation, and that is not an oversight: a paused session
/// still owns the `libseat::Device` entries, and refusing to close them would
/// turn every VT switch into a leak. Closing is a hand-back, not an acquisition.
#[cfg(all(feature = "kms-live", not(test)))]
fn close_session_input_fd(state: &mut LiveSessionState, fd: OwnedFd) -> Result<(), String> {
    let authority_already_revoked = session_authority_devices_are_revoked(state.authority);
    let Some(SessionDeviceOwner { session, .. }) = state.owner.as_mut() else {
        // The session is gone, and with it the device map. Dropping is all that
        // is left and all that is needed.
        return Ok(());
    };
    close_libseat_device(session, fd, authority_already_revoked).map_err(|error| {
        tracing::warn!(%error, "libseat failed to close an input device");
        error
    })
}

#[cfg(all(feature = "kms-live", not(test)))]
fn refuse_pending_input_open(state: &mut LiveSessionState, refusal: InputOpenRefusal) {
    if let Some(pending) = state.pending_input_open.take() {
        let _ = pending.reply.send(Err(refusal.errno()));
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn refuse_pending_session_open(state: &mut LiveSessionState, reason: &'static str) {
    if let Some(pending) = state.pending_open.take() {
        let _ = pending.reply.send(Err(KmsLiveError::Setup(reason.into())));
    }
}

/// The protocol thread's end of the input device protocol.
///
/// Holds two channel senders and one flag, which is the entire point: all three
/// are `Send`, so this can be built here and moved into a factory closure that
/// runs on the protocol thread. A `LibSeatSession` could not make that trip — it
/// is `!Send` by construction and stays on the session thread for the life of
/// the process.
#[cfg(all(feature = "kms-live", not(test)))]
struct LiveInputTransport {
    commands: channel::Sender<LiveSessionCommand>,
    /// Shut for good once an open has timed out.
    gate: InputOpenGate,
    /// The one-way route from this thread to the coordinator.
    ///
    /// An **unbounded** `mpsc::Sender`, so `send` never blocks — which is what
    /// makes it usable from inside a calloop callback that has already waited as
    /// long as it is willing to. The coordinator is blocked on the receiving end
    /// in `wait_for_revocation`, so this wakes it and ends the live operation.
    fatal: LiveCoordinatorSender,
}

/// How long the first session-thread readiness wait may take.
///
/// Runs 1-3 observed 0.2-4.3ms once warm, but the first-ever cold activation was
/// slower and was not quantified. A cold seatd/logind activation therefore
/// keeps the original generous 15-second bound: falsely detaching a healthy
/// session is worse than waiting longer to diagnose a genuine wedge. It must
/// never be unbounded.
#[cfg(any(feature = "kms-live", test))]
const INITIAL_SESSION_READINESS_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one command may wait after the session thread is already ready.
///
/// Runs 1-3 observed 0.2-4.3ms in this warm regime. Three seconds leaves roughly
/// 700x the measured warm worst case. Resume commands use the same per-stage
/// cap inside their separate 30-second overall budget because the compositor
/// and its session thread are necessarily already running.
#[cfg(any(feature = "kms-live", test))]
const RUNNING_SESSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

/// What one bounded session-startup wait established.
#[cfg(any(feature = "kms-live", test))]
#[derive(Debug, Eq, PartialEq)]
enum StartupWait<T> {
    Proceed(T),
    TimeoutWithRevocation(LiveRevocation),
    LostChannel,
}

/// Classify a startup wait without owning a session thread or opening a device.
#[cfg(any(feature = "kms-live", test))]
fn classify_startup_wait<T>(wait: Result<T, mpsc::RecvTimeoutError>) -> StartupWait<T> {
    match wait {
        Ok(value) => StartupWait::Proceed(value),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            StartupWait::TimeoutWithRevocation(LiveRevocation::SessionUnresponsive)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => StartupWait::LostChannel,
    }
}

/// Make a startup reply channel that never strands the session thread in send.
///
/// Capacity one lets a late reply complete after the coordinator's deadline.
/// Dropping a descriptor that arrives too late can leave session-owned state
/// unreclaimed, but the timeout simultaneously requests the deliberate
/// process-lifetime `Detach` leak; reclamation machinery cannot make a wedged
/// foreign session safe to join and would only create another wait on exit.
#[cfg(any(feature = "kms-live", test))]
fn startup_reply_channel<T>() -> (mpsc::SyncSender<T>, Receiver<T>) {
    mpsc::sync_channel(1)
}

/// How long the protocol thread will wait for the session thread to answer an
/// open before giving up on it.
///
/// Runs 1-3 observed 1.0-1.5ms protocol-side and about 1ms session-side. One
/// second is over 600x the measured worst case. Libinput resume can re-enumerate
/// many devices, but this deadline applies independently to each open, so it
/// preserves ample per-device load margin without allowing one wedged open to
/// stall the compositor for five seconds. It must never be unbounded.
#[cfg(any(feature = "kms-live", test))]
const INPUT_OPEN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a device-lifecycle callback will wait for libseat to close an input
/// descriptor.
///
/// Runs 1-3 observed 180us-7.9ms. One second is over 125x the measured worst
/// case and remains a per-device bound during pause reconciliation. A timeout
/// still terminates the compositor and deliberately leaks the session thread
/// for the remaining process lifetime; those fatal-queue semantics are
/// unchanged.
#[cfg(any(feature = "kms-live", test))]
const INPUT_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// How the bounded wait for one input close ended.
#[cfg(any(feature = "kms-live", test))]
#[derive(Debug, Eq, PartialEq)]
enum InputCloseWait {
    Closed,
    CloseFailed(String),
    WaitFailed(mpsc::RecvTimeoutError),
}

/// Classify the input-close acknowledgement without depending on libinput or a
/// live session.
#[cfg(any(feature = "kms-live", test))]
fn classify_input_close_wait(
    wait: Result<Result<(), String>, mpsc::RecvTimeoutError>,
) -> InputCloseWait {
    match wait {
        Ok(Ok(())) => InputCloseWait::Closed,
        Ok(Err(error)) => InputCloseWait::CloseFailed(error),
        Err(error) => InputCloseWait::WaitFailed(error),
    }
}

/// How long teardown will wait for the session thread to answer a shutdown.
///
/// Runs 1-3 observed 698us, 715us and 1.5ms. One second is over 600x the
/// measured worst case. The thread can still wedge after graceful teardown was
/// selected, so exceeding this evidence-based bound keeps the existing fatal
/// behaviour: detach instead of join.
#[cfg(any(feature = "kms-live", test))]
const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// The VT request is advisory to teardown: silence cannot keep the compositor
/// alive or delay the normal close funnel indefinitely.
#[cfg(any(feature = "kms-live", test))]
const VT_SWITCH_REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How many queued revocations teardown will drain before deciding.
///
/// A bound rather than "until empty", because the drain runs while the protocol
/// thread may still be publishing, and a teardown that keeps reading until a
/// producer stops is a teardown that a producer can stall. Every revocation past
/// the limit says the same thing as one of the ones already read.
#[cfg(any(feature = "kms-live", test))]
const LIVE_REVOCATION_DRAIN_LIMIT: usize = 16;

/// Drain the revocations channel for a teardown decision, bounded.
///
/// Returns what was read, and whether the channel still held more than the
/// limit.
///
/// It reads one message past `limit` deliberately. A `take(limit)` alone cannot
/// tell a queue of exactly `limit` messages — which was drained completely —
/// from one that had more, so a caller reporting saturation from the count
/// alone reports it falsely on precisely the boundary it is there to describe.
/// The extra message is kept rather than discarded: dropping a
/// `SessionUnresponsive` on the floor to preserve a round number would be the
/// opposite of what the bound is for.
#[cfg(test)]
fn drain_revocations(
    revocations: &Receiver<LiveRevocation>,
    limit: usize,
) -> (Vec<LiveRevocation>, bool) {
    let mut queued: Vec<LiveRevocation> = std::iter::from_fn(|| revocations.try_recv().ok())
        .take(limit)
        .collect();
    let saturated = queued.len() == limit
        && match revocations.try_recv() {
            Ok(overflow) => {
                queued.push(overflow);
                true
            }
            Err(_) => false,
        };
    (queued, saturated)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn drain_coordinator_events(
    events: &Receiver<LiveCoordinatorEvent>,
    limit: usize,
) -> (Vec<LiveCoordinatorEvent>, bool) {
    let mut queued: Vec<LiveCoordinatorEvent> = std::iter::from_fn(|| events.try_recv().ok())
        .take(limit)
        .collect();
    let saturated = queued.len() == limit
        && match events.try_recv() {
            Ok(overflow) => {
                queued.push(overflow);
                true
            }
            Err(_) => false,
        };
    (queued, saturated)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn queued_revocations(events: &[LiveCoordinatorEvent]) -> Vec<LiveRevocation> {
    events
        .iter()
        .filter_map(|event| match event {
            LiveCoordinatorEvent::Revocation(revocation) => Some(*revocation),
            LiveCoordinatorEvent::Pump(_)
            | LiveCoordinatorEvent::Signal(_)
            | LiveCoordinatorEvent::VtSwitchRequested(_)
            | LiveCoordinatorEvent::PauseRequested { .. }
            | LiveCoordinatorEvent::SessionPaused { .. }
            | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
            | LiveCoordinatorEvent::SessionActivate { .. } => None,
        })
        .collect()
}

/// Record one unanswered input-device operation and publish the fatal exactly
/// on the transition that shuts the shared gate.
///
/// Both open and close waits use this decision. A close has no errno to return,
/// but it must poison later opens and wake the same coordinator when it is the
/// first evidence that the session thread stopped answering.
#[cfg(any(feature = "kms-live", test))]
fn record_input_wait_failure(
    gate: &mut InputOpenGate,
    error: mpsc::RecvTimeoutError,
    fatal: &impl LiveRevocationPublisher,
) -> super::libinput_live::InputOpenWaitOutcome {
    let outcome = gate.record_wait_failure(error);
    if outcome.newly_shut {
        // Unbounded, so this cannot block the thread that has already waited as
        // long as it is willing to. A failure means the coordinator is already
        // gone, which is the outcome being asked for.
        fatal.publish(LiveRevocation::SessionUnresponsive);
    }
    outcome
}

/// Apply the shared gate and fatal transition to a failed input-close wait.
///
/// Kept as the production close path's named decision so its coupling is
/// exercised without constructing libinput or opening a device under tests.
#[cfg(any(feature = "kms-live", test))]
fn record_input_close_wait_failure(
    gate: &mut InputOpenGate,
    error: mpsc::RecvTimeoutError,
    fatal: &impl LiveRevocationPublisher,
) -> super::libinput_live::InputOpenWaitOutcome {
    record_input_wait_failure(gate, error, fatal)
}

/// Turn a failed wait for an input-open reply into the errno to refuse with,
/// logging the transition that makes the shared fatal publication above.
///
/// Extracted from `open_input` so the coupling between the gate transition and
/// the notification is decided somewhere a test can reach with a real channel.
/// Three reviews running, the send itself was the one step of that path no test
/// build compiled; the caller has already dropped its receiver by this point, so
/// everything left of the failure path is here.
#[cfg(any(feature = "kms-live", test))]
fn refuse_input_open_after_wait_failure(
    gate: &mut InputOpenGate,
    error: mpsc::RecvTimeoutError,
    path: &Path,
    fatal: &impl LiveRevocationPublisher,
) -> i32 {
    let outcome = record_input_wait_failure(gate, error, fatal);
    if outcome.newly_shut {
        tracing::error!(
            path = %path.display(),
            timeout_secs = INPUT_OPEN_TIMEOUT.as_secs(),
            "the session thread did not answer an input device open within the deadline; it is \
             alive but not progressing, so device closes, pause and revocation publication, and \
             shutdown cannot be relied on. Signalling the coordinator to end the live operation."
        );
    }
    outcome.errno
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LibinputDeviceTransport for LiveInputTransport {
    fn open_input(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let started = Instant::now();
        if let Some(errno) = self.gate.refusal_before_asking() {
            tracing::debug!(
                path = %path.display(),
                elapsed_us = started.elapsed().as_micros(),
                outcome = "gate-refused",
                errno,
                "libinput device-open round trip finished"
            );
            return Err(errno);
        }
        // Zero-capacity, and the capacity is load-bearing — see
        // `input_open_reply_channel`, which states why and is tested.
        let (reply, result) = input_open_reply_channel();
        // This is the one place the protocol thread waits on the session
        // thread. It is not confined to startup: libinput opens devices from
        // inside `process_events` on hotplug and on resume
        // (`vendor/smithay/src/backend/libinput/mod.rs:735-741`), so a device
        // arriving at runtime stalls a calloop callback for one round trip.
        if self
            .commands
            .send(LiveSessionCommand::OpenInput {
                path: path.to_path_buf(),
                flags,
                reply,
            })
            .is_err()
        {
            let errno = InputOpenRefusal::SessionInactive.errno();
            tracing::debug!(
                path = %path.display(),
                elapsed_us = started.elapsed().as_micros(),
                outcome = "command-channel-closed",
                errno,
                "libinput device-open round trip finished"
            );
            return Err(errno);
        }
        let error = match result.recv_timeout(INPUT_OPEN_TIMEOUT) {
            Ok(outcome) => {
                let errno = outcome.as_ref().err().copied();
                tracing::debug!(
                    path = %path.display(),
                    elapsed_us = started.elapsed().as_micros(),
                    outcome = if outcome.is_ok() { "opened" } else { "refused" },
                    ?errno,
                    "libinput device-open round trip finished"
                );
                return outcome;
            }
            Err(error) => error,
        };
        // Dropped here rather than at the end of the function, so that a late
        // reply is refused from this instant on: with no receiver, the session
        // thread's zero-capacity `send` fails and hands the descriptor back. If
        // this were left to the implicit end-of-scope drop, a sender that woke
        // during the logging below would rendezvous with a receiver nobody is
        // reading, and block until the drop finally released it.
        drop(result);
        let errno = refuse_input_open_after_wait_failure(&mut self.gate, error, path, &self.fatal);
        tracing::debug!(
            path = %path.display(),
            elapsed_us = started.elapsed().as_micros(),
            outcome = match error {
                mpsc::RecvTimeoutError::Timeout => "timed-out",
                mpsc::RecvTimeoutError::Disconnected => "reply-disconnected",
            },
            errno,
            "libinput device-open round trip finished"
        );
        Err(errno)
    }

    fn close_input(&mut self, fd: OwnedFd) {
        let raw_fd = fd.as_raw_fd();
        let started = Instant::now();
        tracing::debug!(raw_fd, "starting libinput device-close round trip");
        let (reply, result) = mpsc::sync_channel(1);
        if self
            .commands
            .send(LiveSessionCommand::CloseInput { fd, reply })
            .is_err()
        {
            tracing::debug!(
                raw_fd,
                elapsed_us = started.elapsed().as_micros(),
                outcome = "command-channel-closed",
                "libinput device-close round trip finished"
            );
            return;
        }

        if self.gate.refusal_before_asking().is_some() {
            tracing::debug!(
                raw_fd,
                elapsed_us = started.elapsed().as_micros(),
                outcome = "sent-after-fatal",
                "libinput device-close round trip finished"
            );
            return;
        }

        match classify_input_close_wait(result.recv_timeout(INPUT_CLOSE_TIMEOUT)) {
            InputCloseWait::Closed => tracing::debug!(
                raw_fd,
                elapsed_us = started.elapsed().as_micros(),
                outcome = "closed",
                "libinput device-close round trip finished"
            ),
            InputCloseWait::CloseFailed(error) => {
                tracing::warn!(raw_fd, %error, "libseat failed to close a libinput device");
                tracing::debug!(
                    raw_fd,
                    elapsed_us = started.elapsed().as_micros(),
                    outcome = "close-failed",
                    "libinput device-close round trip finished"
                );
            }
            InputCloseWait::WaitFailed(error) => {
                let outcome = record_input_close_wait_failure(&mut self.gate, error, &self.fatal);
                if outcome.newly_shut {
                    tracing::error!(
                        raw_fd,
                        timeout_secs = INPUT_CLOSE_TIMEOUT.as_secs(),
                        "the session thread did not answer an input device close within the \
                         deadline; signalling the coordinator to end the live operation"
                    );
                }
                tracing::debug!(
                    raw_fd,
                    elapsed_us = started.elapsed().as_micros(),
                    outcome = match error {
                        mpsc::RecvTimeoutError::Timeout => "timed-out",
                        mpsc::RecvTimeoutError::Disconnected => "reply-disconnected",
                    },
                    "libinput device-close round trip finished"
                );
            }
        }
    }
}

/// Build the factory that will construct libinput on the protocol thread.
///
/// The closure is what crosses the thread boundary, not the backend:
/// `LibinputInputBackend` is `!Send` and is born where it is polled. Everything
/// captured here — two channel senders and a `String` — is `Send`, which is the
/// whole reason the split works.
#[cfg(all(feature = "kms-live", not(test)))]
fn live_input_source(
    commands: channel::Sender<LiveSessionCommand>,
    fatal: LiveCoordinatorSender,
    seat: String,
) -> InputSourceFactory<BoxedLibinputFactory> {
    InputSourceFactory(Box::new(move || {
        let mut libinput =
            Libinput::new_with_udev(ForwardingLibinputInterface(LiveInputTransport {
                commands,
                gate: InputOpenGate::default(),
                fatal,
            }));
        let started = Instant::now();
        let assigned = libinput.udev_assign_seat(&seat);
        tracing::info!(
            seat,
            elapsed_us = started.elapsed().as_micros(),
            success = assigned.is_ok(),
            "libinput seat assignment finished"
        );
        assigned.map_err(|()| -> Box<dyn Error + Send + Sync> {
            format!("libinput could not assign seat {seat}").into()
        })?;
        Ok(LibinputInputBackend::new(libinput))
    }))
}

#[cfg(all(feature = "kms-live", not(test)))]
impl SessionDeviceClient {
    fn event_sender(&self) -> mpsc::Sender<LiveCoordinatorEvent> {
        self.fatal.sender()
    }

    /// Build the input source for this session, without starting it.
    ///
    /// Only the factory crosses the coordinator-to-protocol thread boundary;
    /// libinput itself is constructed later on the protocol thread because it
    /// is not `Send`.
    fn input_source(&self) -> InputSourceFactory<BoxedLibinputFactory> {
        live_input_source(self.commands.clone(), self.fatal.clone(), self.seat.clone())
    }

    /// Examine the revocations already queued after protocol startup, without
    /// losing or reordering them.
    fn adapter_start_decision(&mut self) -> (AdapterStartDecision, Option<CollectedExternalPause>) {
        let (queued, saturated) =
            drain_coordinator_events(&self.events, LIVE_REVOCATION_DRAIN_LIMIT);
        self.deferred_events.extend(queued);
        let revocations = queued_revocations(&self.deferred_events);
        let signal = latched_live_signal().or_else(|| {
            self.deferred_events.iter().find_map(|event| match event {
                LiveCoordinatorEvent::Signal(signal) => Some(*signal),
                LiveCoordinatorEvent::Revocation(_)
                | LiveCoordinatorEvent::Pump(_)
                | LiveCoordinatorEvent::VtSwitchRequested(_)
                | LiveCoordinatorEvent::PauseRequested { .. }
                | LiveCoordinatorEvent::SessionPaused { .. }
                | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
                | LiveCoordinatorEvent::SessionActivate { .. } => None,
            })
        });
        let pause_position = self
            .deferred_events
            .iter()
            .position(|event| matches!(event, LiveCoordinatorEvent::PauseRequested { .. }));
        let first_revocation = self
            .deferred_events
            .iter()
            .position(|event| matches!(event, LiveCoordinatorEvent::Revocation(_)));
        let mut decision = adapter_start_after_events(&self.deferred_events, signal);
        if let Some(pause_position) = pause_position
            && !matches!(decision, AdapterStartDecision::EndSignal(_))
            && first_revocation.is_none_or(|revocation| pause_position < revocation)
        {
            decision = AdapterStartDecision::EndAuthority(LiveRevocation::SessionPause);
        }
        let pause = pause_position.map(|position| match self.deferred_events.remove(position) {
            LiveCoordinatorEvent::PauseRequested {
                generation,
                acknowledgement,
            } => CollectedExternalPause {
                generation,
                acknowledgement,
            },
            _ => unreachable!("pause position was selected by variant"),
        });
        tracing::info!(
            ?decision,
            revocations = ?revocations,
            ?signal,
            saturated,
            "decided whether the live render adapter may start"
        );
        (decision, pause)
    }

    fn open(&self, path: &Path) -> Result<OwnedFd, KmsLiveError> {
        // The initial authority open is already in the running-session regime:
        // the distinct cold readiness wait completed before this client exists.
        self.open_with_timeout(path, RUNNING_SESSION_COMMAND_TIMEOUT)
    }

    fn open_with_timeout(&self, path: &Path, timeout: Duration) -> Result<OwnedFd, KmsLiveError> {
        let started = Instant::now();
        let (reply, result) = startup_reply_channel();
        let outcome = self
            .commands
            .send(LiveSessionCommand::Open {
                path: path.to_path_buf(),
                reply,
            })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))
            .and_then(|()| match classify_startup_wait(result.recv_timeout(timeout)) {
                StartupWait::Proceed(outcome) => outcome,
                StartupWait::TimeoutWithRevocation(revocation) => {
                    let _ = self.fatal.send_revocation(revocation);
                    Err(KmsLiveError::Setup(format!(
                        "live DRM device open did not answer within {}ms; abandoning the session \
                         thread",
                        timeout.as_millis()
                    )))
                }
                StartupWait::LostChannel => Err(KmsLiveError::Setup(
                    "live session DRM device-open reply was lost".into(),
                )),
            });
        tracing::info!(
            path = %path.display(),
            elapsed_us = started.elapsed().as_micros(),
            success = outcome.is_ok(),
            "live DRM device-open wait finished"
        );
        outcome
    }

    fn duplicate_lease(&self) -> Result<MasterDrmLease, KmsLiveError> {
        // As above, initial lease duplication happens only after session
        // readiness, so the measured running-session command bound applies.
        self.duplicate_lease_with_timeout(RUNNING_SESSION_COMMAND_TIMEOUT)
    }

    fn duplicate_lease_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<MasterDrmLease, KmsLiveError> {
        let started = Instant::now();
        let (reply, result) = startup_reply_channel();
        let outcome = self
            .commands
            .send(LiveSessionCommand::Duplicate { reply })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))
            .and_then(
                |()| match classify_startup_wait(result.recv_timeout(timeout)) {
                    StartupWait::Proceed(outcome) => outcome.map_err(KmsLiveError::Setup),
                    StartupWait::TimeoutWithRevocation(revocation) => {
                        let _ = self.fatal.send_revocation(revocation);
                        Err(KmsLiveError::Setup(format!(
                            "live DRM lease duplication did not answer within {}ms; abandoning the \
                         session thread",
                            timeout.as_millis()
                        )))
                    }
                    StartupWait::LostChannel => Err(KmsLiveError::Setup(
                        "live session DRM lease-duplicate reply was lost".into(),
                    )),
                },
            )
            .map(|fd| MasterDrmLease { fd });
        tracing::info!(
            elapsed_us = started.elapsed().as_micros(),
            success = outcome.is_ok(),
            "live DRM lease-duplicate wait finished"
        );
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_scanout(
        &self,
        connector_id: u32,
        connector_identity: &str,
        lifecycle_generation: u64,
        observed_at: Duration,
        old_output_target_existed: bool,
        expected_primary_plane_id: Option<u32>,
    ) -> Result<ResumeScanoutSnapshot, String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(LiveSessionCommand::CaptureScanout {
                connector_id,
                connector_identity: connector_identity.to_owned(),
                lifecycle_generation,
                observed_at,
                old_output_target_existed,
                expected_primary_plane_id,
                reply,
            })
            .map_err(|_| {
                "live session command channel closed during scanout capture".to_string()
            })?;
        result
            .recv_timeout(RUNNING_SESSION_COMMAND_TIMEOUT)
            .map_err(|error| format!("live scanout capture did not answer: {error}"))?
    }

    fn request_vt_switch(&self, vt: u8) -> VtSwitchAsk {
        self.request_vt_switch_inner(vt, false)
    }

    fn request_self_vt_switch(&self, vt: u8) -> VtSwitchAsk {
        self.request_vt_switch_inner(vt, true)
    }

    fn request_vt_switch_inner(&self, vt: u8, confirm_self_pause: bool) -> VtSwitchAsk {
        let started = Instant::now();
        let (reply, result) = mpsc::sync_channel(1);
        let outcome = if self
            .commands
            .send(LiveSessionCommand::SwitchVt {
                vt,
                confirm_self_pause,
                reply,
            })
            .is_err()
        {
            VtSwitchAsk::Unsent
        } else {
            vt_switch_ask_after_wait(result.recv_timeout(VT_SWITCH_REPLY_TIMEOUT))
        };
        tracing::info!(
            vt,
            ?outcome,
            elapsed_us = started.elapsed().as_micros(),
            deadline_ms = VT_SWITCH_REPLY_TIMEOUT.as_millis(),
            "live VT-switch request wait finished"
        );
        outcome
    }

    fn begin_self_switch(&self, generation: u64) -> Result<(), KmsLiveError> {
        let (reply, result) = startup_reply_channel();
        self.commands
            .send(LiveSessionCommand::BeginSelfSwitch { generation, reply })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))?;
        match classify_startup_wait(result.recv_timeout(RUNNING_SESSION_COMMAND_TIMEOUT)) {
            StartupWait::Proceed(result) => result,
            StartupWait::TimeoutWithRevocation(revocation) => {
                let _ = self.fatal.send_revocation(revocation);
                Err(KmsLiveError::Setup(
                    "live session did not begin the self-switch within its deadline".into(),
                ))
            }
            StartupWait::LostChannel => Err(KmsLiveError::Setup(
                "live self-switch preparation reply was lost".into(),
            )),
        }
    }

    fn close_original(&self) -> Result<(), KmsLiveError> {
        let (reply, result) = startup_reply_channel();
        self.commands
            .send(LiveSessionCommand::CloseOriginal { reply })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))?;
        match classify_startup_wait(result.recv_timeout(RUNNING_SESSION_COMMAND_TIMEOUT)) {
            StartupWait::Proceed(result) => result.map_err(KmsLiveError::Setup),
            StartupWait::TimeoutWithRevocation(revocation) => {
                let _ = self.fatal.send_revocation(revocation);
                Err(KmsLiveError::Setup(
                    "live DRM original close did not answer within its deadline".into(),
                ))
            }
            StartupWait::LostChannel => Err(KmsLiveError::Setup(
                "live DRM original-close reply was lost".into(),
            )),
        }
    }

    fn begin_resume(&self, timeout: Duration) -> Result<u64, KmsLiveError> {
        let (reply, result) = startup_reply_channel();
        self.commands
            .send(LiveSessionCommand::BeginResume { reply })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))?;
        match classify_startup_wait(result.recv_timeout(timeout)) {
            StartupWait::Proceed(result) => result.map_err(KmsLiveError::Setup),
            StartupWait::TimeoutWithRevocation(revocation) => {
                let _ = self.fatal.send_revocation(revocation);
                Err(KmsLiveError::Setup(
                    "live session did not begin resume within its deadline".into(),
                ))
            }
            StartupWait::LostChannel => {
                Err(KmsLiveError::Setup("live resume reply was lost".into()))
            }
        }
    }

    fn return_paused(
        &self,
        generation: u64,
        cause: LivePauseCause,
        timeout: Duration,
    ) -> Result<(), KmsLiveError> {
        let (reply, result) = startup_reply_channel();
        self.commands
            .send(LiveSessionCommand::ReturnPaused {
                generation,
                cause,
                reply,
            })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))?;
        match classify_startup_wait(result.recv_timeout(timeout)) {
            StartupWait::Proceed(result) => result.map_err(KmsLiveError::Setup),
            StartupWait::TimeoutWithRevocation(revocation) => {
                let _ = self.fatal.send_revocation(revocation);
                Err(KmsLiveError::Setup(
                    "live session did not return to paused within its deadline".into(),
                ))
            }
            StartupWait::LostChannel => Err(KmsLiveError::Setup(
                "live return-to-paused reply was lost".into(),
            )),
        }
    }

    fn finish_resume(&self, generation: u64, timeout: Duration) -> Result<(), KmsLiveError> {
        let (reply, result) = startup_reply_channel();
        self.commands
            .send(LiveSessionCommand::FinishResume { generation, reply })
            .map_err(|_| KmsLiveError::Setup("live session command channel closed".into()))?;
        match classify_startup_wait(result.recv_timeout(timeout)) {
            StartupWait::Proceed(result) => result.map_err(KmsLiveError::Setup),
            StartupWait::TimeoutWithRevocation(revocation) => {
                let _ = self.fatal.send_revocation(revocation);
                Err(KmsLiveError::Setup(
                    "live session did not finish resume within its deadline".into(),
                ))
            }
            StartupWait::LostChannel => Err(KmsLiveError::Setup(
                "live finish-resume reply was lost".into(),
            )),
        }
    }

    fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        if !self.deferred_events.is_empty() {
            return Ok(Some(self.deferred_events.remove(0)));
        }
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(KmsLiveError::Setup(
                "live session dispatch thread stopped".into(),
            )),
        }
    }

    fn wait_for_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        if !self.deferred_events.is_empty() {
            return Ok(Some(self.deferred_events.remove(0)));
        }
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(KmsLiveError::Setup(
                "live session dispatch thread stopped".into(),
            )),
        }
    }

    /// End the session thread, however the coordinator decided to.
    ///
    /// The decision is re-taken here against anything the revocations channel
    /// still holds, because the coordinator read that channel once and a fatal
    /// notification can have arrived behind the message it woke on. See
    /// [`teardown_upgraded_by`].
    fn close(mut self, teardown: SessionTeardown) -> Result<(), KmsLiveError> {
        let (newly_queued, saturated) =
            drain_coordinator_events(&self.events, LIVE_REVOCATION_DRAIN_LIMIT);
        self.deferred_events.extend(newly_queued);
        let events_read = self.deferred_events.len();
        let queued = queued_revocations(&self.deferred_events);
        if saturated {
            // Said rather than passed over, and said no more strongly than the
            // drain can establish: reading one past the limit proves more was
            // queued than the limit allows for, and nothing about whether
            // anything is still queued behind what was read.
            tracing::warn!(
                limit = LIVE_REVOCATION_DRAIN_LIMIT,
                read = events_read,
                "the revocations channel held more at teardown than the drain limit; the teardown \
                 decision was taken on the first messages read and nothing published past them \
                 was examined"
            );
        }
        let (upgraded, decision_failure) = resolve_session_close(teardown, &queued);
        tracing::info!(
            chosen = ?teardown,
            ?upgraded,
            revocations = ?queued,
            saturated,
            "resolved live session teardown"
        );
        let closed = match upgraded {
            SessionTeardown::Graceful => self.shutdown(),
            SessionTeardown::Detach => {
                self.abandon();
                Ok(())
            }
        };
        combine_live_results(decision_failure, closed)
    }

    fn shutdown(mut self) -> Result<(), KmsLiveError> {
        let (ask, close) = self.ask_for_shutdown();
        match session_exit_after_shutdown(ask) {
            SessionExit::Joinable => {
                let joined = self
                    .thread
                    .take()
                    .expect("live session thread exists until shutdown")
                    .join()
                    .map_err(|_| {
                        KmsLiveError::Setup("live session dispatch thread panicked".into())
                    });
                combine_live_results(close, joined)
            }
            SessionExit::Wedged => {
                self.detach_thread(Some(ask));
                close
            }
        }
    }

    /// Ask the session thread to shut down, and wait — bounded — for it to say
    /// it has.
    ///
    /// The bound is the whole point: a wedged thread never answers, and an
    /// unbounded wait here would move the hang out of the input path and into
    /// teardown rather than removing it. The coordinator can reach this even for
    /// a session it believes healthy, because a thread can wedge after its
    /// teardown was already chosen.
    fn ask_for_shutdown(&self) -> (ShutdownAsk, Result<(), KmsLiveError>) {
        let (reply, result) = mpsc::sync_channel(1);
        if self
            .commands
            .send(LiveSessionCommand::Shutdown { reply })
            .is_err()
        {
            return (
                ShutdownAsk::Unsent,
                Err(KmsLiveError::Setup(
                    "live session command channel closed".into(),
                )),
            );
        }
        let started = Instant::now();
        let classified = shutdown_ask_after_wait(result.recv_timeout(SESSION_SHUTDOWN_TIMEOUT));
        tracing::info!(
            ask = ?classified.0,
            elapsed_us = started.elapsed().as_micros(),
            deadline_us = SESSION_SHUTDOWN_TIMEOUT.as_micros(),
            "live session shutdown wait finished"
        );
        classified
    }

    /// Give up on the session thread instead of shutting it down.
    ///
    /// For a thread already known to be alive but not progressing. `shutdown`
    /// would send it a command and then spend the whole shutdown deadline
    /// establishing what the fatal notification already said, before detaching
    /// anyway. `Drop` is worse — it joins unconditionally — which is why the
    /// `JoinHandle` is taken here and dropped: detaching it leaves `Drop` with
    /// nothing to join and nothing to send.
    ///
    /// The thread and whatever it holds — the libseat session, its device map —
    /// are deliberately leaked. There is no way to reclaim them from a thread
    /// stuck inside a foreign library, and the live operation is ending anyway.
    fn abandon(mut self) {
        self.detach_thread(None);
    }

    /// Take the `JoinHandle` and drop it, leaving `Drop` nothing to wait on.
    ///
    /// Shared by [`abandon`](Self::abandon), which never asked, and by the
    /// wedged arm of [`shutdown`](Self::shutdown), which reaches the same
    /// conclusion the long way round: a thread that did not acknowledge must
    /// not be joined. `ask` records which, because "it never answered" and "it
    /// was never asked" are different faults with the same remedy.
    fn detach_thread(&mut self, ask: Option<ShutdownAsk>) {
        let detached = self.thread.take().is_some();
        tracing::error!(
            detached,
            ?ask,
            "abandoning the live session thread without waiting for it; whatever it still holds \
             — its libseat session, its device map — is leaked for the remaining life of the \
             process"
        );
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl Drop for SessionDeviceClient {
    fn drop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        // The same bounded ask `shutdown` makes, for the paths that never
        // reached `close`: an early error, or an unwinding panic. It used to be
        // an unconditional `join`, which is precisely the wait this rung exists
        // to bound — a fallback that hangs is worse than the failure it is
        // falling back from.
        let (ask, _) = self.ask_for_shutdown();
        match session_exit_after_shutdown(ask) {
            SessionExit::Joinable => {
                let thread = self
                    .thread
                    .take()
                    .expect("the handle was present immediately above");
                if thread.join().is_err() {
                    tracing::error!(
                        "live session dispatch thread panicked during fallback shutdown"
                    );
                }
            }
            SessionExit::Wedged => self.detach_thread(Some(ask)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsLiveRefusal {
    SubcommandNotFirst,
    UnknownArgument,
    MissingDevice,
    DuplicateDevice,
    InvalidDevice,
    MissingConnector,
    DuplicateConnector,
    InvalidConnector,
    MissingScale,
    DuplicateScale,
    InvalidScale,
    NonPositiveScale,
    Non120thScale,
    DuplicateFirstLight,
    DuplicateSsd,
    DuplicateNoSsd,
    SsdNoSsdConflict,
    MissingChrome,
    DuplicateChrome,
    InvalidChrome,
    NoSsdChromeConflict,
    DuplicateKmsConfirm,
    DecorationFirstLightConflict,
    MissingPresentation,
    DuplicatePresentation,
    InvalidPresentation,
    DirectDisplayRetired,
    FeatureDisabled,
    ReleaseBuildRequired,
    #[cfg(any(feature = "kms-live", test))]
    TtyOpenFailed,
    VtObservationUnavailable,
    TtyNotCharacterDevice,
    TtyNotKernelAlias,
    TtyNotForegroundProcessGroup,
    TtyNotVirtualTerminal,
    TtyNotActive,
    VtChangedSinceAuthorisation,
    DeviceObservationUnavailable,
    DeviceObservationTargetMismatch,
    DeviceNotCharacterDevice,
    DeviceNotPrimaryNode,
    DeviceMissingUdevIdentity,
    DeviceStableIdentityUnavailable,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    DeviceStableIdentityChanged,
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    DeviceCanonicalIdentityChanged,
    DeviceRdevMismatch,
    #[cfg(any(feature = "kms-live", test))]
    SessionInactiveBeforeAuthorityOpen,
    #[cfg(any(feature = "kms-live", test))]
    RevokedBeforeAuthorityOpen,
    #[cfg(any(feature = "kms-live", test))]
    DrmNodeOpenFailed,
    #[cfg(any(feature = "kms-live", test))]
    DrmNodeObservationUnavailable,
    #[cfg(any(feature = "kms-live", test))]
    ConnectorBoundaryScanFailed,
    ConnectorNotPresent,
    #[cfg(any(feature = "kms-live", test))]
    TtyInputFlushFailed,
    #[cfg(any(feature = "kms-live", test))]
    TtyLegacyInjectionStateUnavailable,
    #[cfg(any(feature = "kms-live", test))]
    TtyLegacyInjectionEnabled,
    #[cfg(any(feature = "kms-live", test))]
    ConfirmationNonceUnavailable,
    #[cfg(any(feature = "kms-live", test))]
    ConfirmationReadFailed,
    ConfirmationMismatch,
    #[cfg(any(feature = "kms-live", test))]
    DeviceIncarnationOpenFailed,
    #[cfg(any(feature = "kms-live", test))]
    DeviceIncarnationReadFailed,
    #[cfg(any(feature = "kms-live", test))]
    DeviceIncarnationGone,
    #[cfg(any(feature = "kms-live", test))]
    DeviceIncarnationChanged,
    #[cfg(any(not(feature = "kms-live"), test))]
    LiveBodyUnavailable,
}

impl KmsLiveRefusal {
    pub(crate) const fn reason_code(self) -> &'static str {
        match self {
            Self::SubcommandNotFirst => "kms-live-subcommand-not-first",
            Self::UnknownArgument => "kms-live-unknown-argument",
            Self::MissingDevice => "kms-live-device-missing",
            Self::DuplicateDevice => "kms-live-device-duplicate",
            Self::InvalidDevice => "kms-live-device-invalid",
            Self::MissingConnector => "kms-live-connector-missing",
            Self::DuplicateConnector => "kms-live-connector-duplicate",
            Self::InvalidConnector => "kms-live-connector-invalid",
            Self::MissingScale => "kms-live-scale-missing",
            Self::DuplicateScale => "kms-live-scale-duplicate",
            Self::InvalidScale => "kms-live-scale-invalid",
            Self::NonPositiveScale => "kms-live-scale-non-positive",
            Self::Non120thScale => "kms-live-scale-not-exact-120th",
            Self::DuplicateFirstLight => "kms-live-first-light-duplicate",
            Self::DuplicateSsd => "kms-live-ssd-duplicate",
            Self::DuplicateNoSsd => "kms-live-no-ssd-duplicate",
            Self::SsdNoSsdConflict => "kms-live-ssd-no-ssd-conflict",
            Self::MissingChrome => "kms-live-chrome-missing",
            Self::DuplicateChrome => "kms-live-chrome-duplicate",
            Self::InvalidChrome => "kms-live-chrome-invalid",
            Self::NoSsdChromeConflict => "kms-live-no-ssd-chrome-conflict",
            Self::DuplicateKmsConfirm => "kms-live-kms-confirm-duplicate",
            Self::DecorationFirstLightConflict => "kms-live-decoration-first-light-conflict",
            Self::MissingPresentation => "kms-live-presentation-missing",
            Self::DuplicatePresentation => "kms-live-presentation-duplicate",
            Self::InvalidPresentation => "kms-live-presentation-invalid",
            Self::DirectDisplayRetired => "kms-live-direct-display-retired",
            Self::FeatureDisabled => "kms-live-feature-disabled",
            Self::ReleaseBuildRequired => "kms-live-release-build-required",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyOpenFailed => "kms-live-tty-open-failed",
            Self::VtObservationUnavailable => "kms-live-vt-observation-unavailable",
            Self::TtyNotCharacterDevice => "kms-live-tty-not-character-device",
            Self::TtyNotKernelAlias => "kms-live-tty-not-kernel-alias",
            Self::TtyNotForegroundProcessGroup => "kms-live-tty-not-foreground-pgrp",
            Self::TtyNotVirtualTerminal => "kms-live-tty-not-virtual-terminal",
            Self::TtyNotActive => "kms-live-tty-not-active",
            Self::VtChangedSinceAuthorisation => "kms-live-vt-changed-since-authorisation",
            Self::DeviceObservationUnavailable => "kms-live-device-observation-unavailable",
            Self::DeviceObservationTargetMismatch => "kms-live-device-observation-target-mismatch",
            Self::DeviceNotCharacterDevice => "kms-live-device-not-character-device",
            Self::DeviceNotPrimaryNode => "kms-live-device-not-primary-node",
            Self::DeviceMissingUdevIdentity => "kms-live-device-missing-udev-identity",
            Self::DeviceStableIdentityUnavailable => "kms-live-device-stable-identity-unavailable",
            Self::DeviceStableIdentityChanged => "kms-live-device-stable-identity-changed",
            Self::DeviceCanonicalIdentityChanged => "kms-live-device-canonical-identity-changed",
            Self::DeviceRdevMismatch => "kms-live-device-rdev-mismatch",
            #[cfg(any(feature = "kms-live", test))]
            Self::SessionInactiveBeforeAuthorityOpen => {
                "kms-live-session-inactive-before-authority-open"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::RevokedBeforeAuthorityOpen => "kms-live-revoked-before-authority-open",
            #[cfg(any(feature = "kms-live", test))]
            Self::DrmNodeOpenFailed => "kms-live-drm-open-failed",
            #[cfg(any(feature = "kms-live", test))]
            Self::DrmNodeObservationUnavailable => "kms-live-drm-fstat-failed",
            #[cfg(any(feature = "kms-live", test))]
            Self::ConnectorBoundaryScanFailed => "kms-live-connector-boundary-scan-failed",
            Self::ConnectorNotPresent => "kms-live-connector-not-present",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyInputFlushFailed => "kms-live-tty-input-flush-failed",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyLegacyInjectionStateUnavailable => {
                "kms-live-tty-legacy-injection-state-unavailable"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyLegacyInjectionEnabled => "kms-live-tty-legacy-injection-enabled",
            #[cfg(any(feature = "kms-live", test))]
            Self::ConfirmationNonceUnavailable => "kms-live-confirmation-nonce-unavailable",
            #[cfg(any(feature = "kms-live", test))]
            Self::ConfirmationReadFailed => "kms-live-confirmation-read-failed",
            Self::ConfirmationMismatch => "kms-live-confirmation-mismatch",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationOpenFailed => "kms-live-device-incarnation-open-failed",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationReadFailed => "kms-live-device-incarnation-read-failed",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationGone => "kms-live-device-incarnation-gone",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationChanged => "kms-live-device-incarnation-changed",
            #[cfg(any(not(feature = "kms-live"), test))]
            Self::LiveBodyUnavailable => LIVE_BODY_UNAVAILABLE_REASON,
        }
    }
}

impl fmt::Display for KmsLiveRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::SubcommandNotFirst => "`kms-live` must be the first argument",
            Self::UnknownArgument => "the live KMS command contains an unknown argument",
            Self::MissingDevice => "`--device` and its value are required",
            Self::DuplicateDevice => "`--device` may only be supplied once",
            Self::InvalidDevice => "`--device` must be an absolute UTF-8 path",
            Self::MissingConnector => "`--connector` and its value are required",
            Self::DuplicateConnector => "`--connector` may only be supplied once",
            Self::InvalidConnector => {
                "`--connector` must contain only ASCII letters, digits or '-'"
            }
            Self::MissingScale => "`--scale` requires an exact decimal value",
            Self::DuplicateScale => "`--scale` may only be supplied once",
            Self::InvalidScale => "`--scale` must be a finite plain decimal",
            Self::NonPositiveScale => "`--scale` must be greater than zero",
            Self::Non120thScale => "`--scale` must be exactly representable in 120ths",
            Self::DuplicateFirstLight => "`--first-light` may only be supplied once",
            Self::DuplicateSsd => "`--ssd` may only be supplied once",
            Self::DuplicateNoSsd => "`--no-ssd` may only be supplied once",
            Self::SsdNoSsdConflict => "`--ssd` cannot be combined with `--no-ssd`",
            Self::MissingChrome => "`--chrome` requires a style",
            Self::DuplicateChrome => "`--chrome` may only be supplied once",
            Self::InvalidChrome => "`--chrome` must be one of mac, win11 or cosmix",
            Self::NoSsdChromeConflict => "`--chrome` cannot be combined with `--no-ssd`",
            Self::DuplicateKmsConfirm => "`--kms-confirm` may only be supplied once",
            Self::DecorationFirstLightConflict => {
                "`--first-light` cannot be combined with `--ssd` or `--chrome`"
            }
            Self::MissingPresentation => {
                "`--presentation` requires `atomic` (`direct-display` is retired)"
            }
            Self::DuplicatePresentation => "`--presentation` may only be supplied once",
            Self::InvalidPresentation => {
                "`--presentation` must be `atomic` (`direct-display` is retired)"
            }
            Self::DirectDisplayRetired => {
                "Vulkan direct-display presentation was permanently retired; use `atomic`"
            }
            Self::FeatureDisabled => "binary was not built with the non-default `kms-live` feature",
            Self::ReleaseBuildRequired => "live KMS requires Cargo's release profile",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyOpenFailed => "the controlling-TTY alias could not be opened safely",
            Self::VtObservationUnavailable => {
                "the controlling terminal or VT_GETSTATE is unavailable"
            }
            Self::TtyNotCharacterDevice => {
                "the opened controlling terminal is not a character device"
            }
            Self::TtyNotKernelAlias => "the opened node is not the kernel /dev/tty alias",
            Self::TtyNotForegroundProcessGroup => {
                "the caller is not the controlling terminal's foreground process group"
            }
            Self::TtyNotVirtualTerminal => "the controlling terminal is not a real Linux VT",
            Self::TtyNotActive => "the controlling VT is not the kernel's active VT",
            Self::VtChangedSinceAuthorisation => "the controlling VT changed after authorisation",
            Self::DeviceObservationUnavailable => {
                "the requested DRM device identity could not be observed"
            }
            Self::DeviceObservationTargetMismatch => {
                "the device observation belongs to another path"
            }
            Self::DeviceNotCharacterDevice => "the requested DRM device is not a character device",
            Self::DeviceNotPrimaryNode => "the requested DRM device is not a primary card node",
            Self::DeviceMissingUdevIdentity => "udev did not report the requested primary DRM node",
            Self::DeviceStableIdentityUnavailable => {
                "the DRM node has no stable parent-device identity"
            }
            Self::DeviceStableIdentityChanged => {
                "the DRM parent-device identity changed after confirmation"
            }
            Self::DeviceCanonicalIdentityChanged => {
                "the DRM node the request resolves to changed after confirmation"
            }
            Self::DeviceRdevMismatch => "the actual DRM node has a different dev_t",
            #[cfg(any(feature = "kms-live", test))]
            Self::SessionInactiveBeforeAuthorityOpen => {
                "the libseat session is inactive before the authority open"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::RevokedBeforeAuthorityOpen => {
                "the live target was revoked before the authority open"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::DrmNodeOpenFailed => "the authorised DRM node could not be opened",
            #[cfg(any(feature = "kms-live", test))]
            Self::DrmNodeObservationUnavailable => "the opened DRM node could not be fstat'd",
            #[cfg(any(feature = "kms-live", test))]
            Self::ConnectorBoundaryScanFailed => {
                "the requested connector could not be rescanned through the opened DRM node"
            }
            Self::ConnectorNotPresent => "the requested connector is not present for the card",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyInputFlushFailed => "the controlling tty input queue could not be flushed",
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyLegacyInjectionStateUnavailable => {
                "the kernel legacy TIOCSTI state could not be read"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::TtyLegacyInjectionEnabled => {
                "the kernel permits unprivileged legacy TIOCSTI injection"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::ConfirmationNonceUnavailable => {
                "the confirmation nonce could not be generated securely"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::ConfirmationReadFailed => {
                "the confirmation could not be read from the controlling tty"
            }
            Self::ConfirmationMismatch => "the typed confirmation does not match the fresh code",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationOpenFailed => {
                "the authorised DRM card incarnation could not be held"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationReadFailed => {
                "the authorised DRM card incarnation could not be read"
            }
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationGone => "the authorised DRM card incarnation was unregistered",
            #[cfg(any(feature = "kms-live", test))]
            Self::DeviceIncarnationChanged => {
                "the opened DRM card is not the authorised incarnation"
            }
            #[cfg(any(not(feature = "kms-live"), test))]
            Self::LiveBodyUnavailable => "the live DRM body is unavailable in this build",
        };
        formatter.write_str(detail)
    }
}

impl Error for KmsLiveRefusal {}

pub(crate) fn authorise(argv: &[OsString]) -> Result<KmsLiveGrant, KmsLiveRefusal> {
    let request = parse_request(argv)?;
    let build = BuildProfile::current();
    validate_build(build)?;
    validate_presentation_backend(request.presentation_backend)?;

    #[cfg(any(not(feature = "kms-live"), test))]
    {
        let unavailable = DeviceIdentity::unavailable_for(request.device);
        match decide(
            argv,
            "",
            &[0; CONFIRMATION_NONCE_BYTES],
            VtState::default(),
            build,
            &unavailable,
        ) {
            Ok(_) => unreachable!("a build without kms-live cannot validate"),
            Err(refusal) => Err(refusal),
        }
    }

    #[cfg(all(feature = "kms-live", not(test)))]
    {
        let tty = open_controlling_tty()?;
        let tty_kernel: Rc<dyn TtyKernelCalls> = Rc::new(LibcTtyKernelCalls);
        let platform: Rc<dyn GrantPlatform> = Rc::new(LinuxPlatform {
            tty_kernel: Rc::clone(&tty_kernel),
        });
        let mut confirmation = TtyConfirmationSource { tty_kernel };
        authorise_observed(request, build, tty, platform, &mut confirmation)
    }
}

// The argv-only wrapper is reachable only from the non-kms-live `authorise`
// arm and from tests; in an `--all-features` non-test build every caller is
// cfg'd out, so gate the wrapper to match or it reads as dead code.
#[cfg(any(not(feature = "kms-live"), test))]
fn decide(
    argv: &[OsString],
    confirmation: &str,
    nonce: &[u8; CONFIRMATION_NONCE_BYTES],
    vt: VtState,
    build: BuildProfile,
    device: &DeviceIdentity,
) -> Result<KmsLiveDecision, KmsLiveRefusal> {
    decide_request(parse_request(argv)?, confirmation, nonce, vt, build, device)
}

/// The confirmation/observation decision over an *already parsed* request.
///
/// `authorise_observed` calls this with the very same `request` it used to
/// decide whether to display and read a nonce, so the "was confirmation
/// requested?" question has a single source of truth: the prompt/read branch and
/// the comparison branch below can never disagree about `confirm`. The argv-only
/// [`decide`] wrapper exists for callers and tests that hold only the raw argv
/// (and rely on the re-parse to surface parse refusals); it must pass the request
/// it just parsed, never a hand-built one that could contradict `argv`.
fn decide_request(
    request: KmsLiveRequest,
    confirmation: &str,
    nonce: &[u8; CONFIRMATION_NONCE_BYTES],
    vt: VtState,
    build: BuildProfile,
    device: &DeviceIdentity,
) -> Result<KmsLiveDecision, KmsLiveRefusal> {
    let decision = validate_observations(request, vt, build, device)?;
    // The typed-nonce challenge is opt-in (`--kms-confirm`). When it was not
    // requested, no code was displayed or read, so there is nothing to compare;
    // the device/VT/connector binding above and the post-observation continuity
    // checks in `authorise_observed` still run unconditionally. This single-use
    // process refuses on the first mismatch, so a constant-time comparison
    // provides no useful protection from iteration.
    if decision.request.confirm && confirmation != confirmation_code(nonce) {
        return Err(KmsLiveRefusal::ConfirmationMismatch);
    }
    Ok(decision)
}

#[cfg(test)]
pub(crate) fn refusal_reason_for_test(argv: &[OsString]) -> &'static str {
    decide(
        argv,
        "unused-before-parse",
        &[0; CONFIRMATION_NONCE_BYTES],
        VtState::default(),
        BuildProfile {
            kms_live_feature: true,
            release: true,
        },
        &DeviceIdentity {
            observation_available: false,
            observed_for: PathBuf::new(),
            canonical_path: None,
            node_is_character_device: false,
            node_is_primary_drm: false,
            node_rdev: 0,
            udev_rdev: None,
            stable_device_path: None,
            connectors: BTreeSet::new(),
        },
    )
    .expect_err("test seam is only used for refusal paths")
    .reason_code()
}

fn validate_build(build: BuildProfile) -> Result<(), KmsLiveRefusal> {
    if !build.kms_live_feature {
        return Err(KmsLiveRefusal::FeatureDisabled);
    }
    if !build.release {
        return Err(KmsLiveRefusal::ReleaseBuildRequired);
    }
    Ok(())
}

fn validate_presentation_backend(
    presentation_backend: PresentationBackend,
) -> Result<(), KmsLiveRefusal> {
    match presentation_backend {
        PresentationBackend::Atomic => Ok(()),
    }
}

fn validate_vt(vt: VtState) -> Result<u16, KmsLiveRefusal> {
    if !vt.observation_available || vt.active_vt.is_none() {
        return Err(KmsLiveRefusal::VtObservationUnavailable);
    }
    if !vt.tty_is_character_device {
        return Err(KmsLiveRefusal::TtyNotCharacterDevice);
    }
    if vt.tty_alias_rdev != libc::makedev(TTYAUX_MAJOR, TTY_ALIAS_MINOR) {
        return Err(KmsLiveRefusal::TtyNotKernelAlias);
    }
    if !vt.foreground_process_group {
        return Err(KmsLiveRefusal::TtyNotForegroundProcessGroup);
    }
    if vt.tty_major != LINUX_VT_MAJOR || !(MIN_LINUX_VT..=MAX_LINUX_VT).contains(&vt.tty_minor) {
        return Err(KmsLiveRefusal::TtyNotVirtualTerminal);
    }
    let active_vt = vt.active_vt.expect("VT availability checked above");
    if u32::from(active_vt) != vt.tty_minor {
        return Err(KmsLiveRefusal::TtyNotActive);
    }
    Ok(active_vt)
}

fn validate_observations(
    request: KmsLiveRequest,
    vt: VtState,
    build: BuildProfile,
    device: &DeviceIdentity,
) -> Result<KmsLiveDecision, KmsLiveRefusal> {
    validate_build(build)?;
    validate_presentation_backend(request.presentation_backend)?;
    let fresh_vt = validate_vt(vt)?;
    if !device.observation_available || device.canonical_path.is_none() {
        return Err(KmsLiveRefusal::DeviceObservationUnavailable);
    }
    if device.observed_for != request.device {
        return Err(KmsLiveRefusal::DeviceObservationTargetMismatch);
    }
    if !device.node_is_character_device {
        return Err(KmsLiveRefusal::DeviceNotCharacterDevice);
    }
    if !device.node_is_primary_drm {
        return Err(KmsLiveRefusal::DeviceNotPrimaryNode);
    }
    let Some(udev_rdev) = device.udev_rdev else {
        return Err(KmsLiveRefusal::DeviceMissingUdevIdentity);
    };
    if device.node_rdev != udev_rdev {
        return Err(KmsLiveRefusal::DeviceRdevMismatch);
    }
    let Some(stable_device_path) = device.stable_device_path.clone() else {
        return Err(KmsLiveRefusal::DeviceStableIdentityUnavailable);
    };
    if !device.connectors.contains(&request.connector) {
        return Err(KmsLiveRefusal::ConnectorNotPresent);
    }
    Ok(KmsLiveDecision {
        request,
        canonical_device: device.canonical_path.clone().expect("checked above"),
        vt: fresh_vt,
        stable_device_path,
        drm_device: device.node_rdev,
    })
}

/// Authorise a live takeover against observations made on both sides of the
/// (optional) operator confirmation.
///
/// The typed-nonce challenge is **opt-in** via `--kms-confirm`
/// ([`KmsLiveRequest::confirm`]). By default a live takeover runs *unattended* —
/// no code is displayed or read — so an agent can drive it with no human at the
/// glass; the takeover is announced loudly on the tracing log instead. Pass
/// `--kms-confirm` to require the typed code, the guard rail for a human
/// operator who wants it.
///
/// When enabled, the typed code's load-bearing job is freshness against input
/// injected after the input flush. Four random bytes give 32 bits. Because a
/// mismatch is a terminal refusal, a blind injector gets one attempt, with a
/// success probability of approximately 2^-32. An injector that can observe
/// terminal output is explicitly outside this interlock's boundary.
///
/// The device, VT and connector remain bound **independently of the typed code
/// and regardless of whether it was requested**: the input flush, the legacy
/// TIOCSTI refusal, and the post-confirmation observation all run
/// unconditionally. That observation rejects VT drift with
/// [`KmsLiveRefusal::VtChangedSinceAuthorisation`], rejects stable-device drift
/// with [`KmsLiveRefusal::DeviceStableIdentityChanged`], and repeats the full
/// device and connector validation against refreshed observations.
#[cfg(any(feature = "kms-live", test))]
fn authorise_observed(
    request: KmsLiveRequest,
    build: BuildProfile,
    tty: OwnedFd,
    platform: Rc<dyn GrantPlatform>,
    confirmation: &mut dyn ConfirmationIo,
) -> Result<KmsLiveGrant, KmsLiveRefusal> {
    let initial_vt = platform.observe_vt(tty.as_fd());
    let initial_device = platform.observe_device(&request);
    let preliminary = validate_observations(request.clone(), initial_vt, build, &initial_device)?;
    // Refusing legacy TIOCSTI is defence in depth: CAP_SYS_ADMIN bypasses the
    // sysctl.
    if platform.legacy_tiocsti_enabled()? {
        return Err(KmsLiveRefusal::TtyLegacyInjectionEnabled);
    }
    let incarnation = platform.hold_device_incarnation(&initial_device)?;

    // Flush pending tty input in both modes: it is the interlock's hygiene
    // against input queued before this point, independent of whether a typed
    // challenge follows.
    confirmation.flush_input(tty.as_fd())?;
    let mut nonce = [0_u8; CONFIRMATION_NONCE_BYTES];
    let typed = if preliminary.request.confirm {
        platform.fill_confirmation_nonce(&mut nonce)?;
        let intent = format!(
            "About to take DRM master of {} ({}) on tty{} with requested scale {}; the physical mode will be selected after confirmation.",
            preliminary.canonical_device.display(),
            preliminary.request.connector,
            preliminary.vt,
            preliminary.request.output_scale,
        );
        let expected_code = confirmation_code(&nonce);
        confirmation.display_prompt(tty.as_fd(), &intent, &expected_code)?;
        confirmation.read_line(tty.as_fd())?
    } else {
        // Unattended: no operator at the glass answers a nonce. The nonce stays
        // zeroed and `decide` skips the comparison for this request; every other
        // guard (VT continuity, device incarnation and canonical/stable
        // identity, connector presence) still runs below. Announce loudly so the
        // takeover is legible even without an interactive prompt — via BOTH a
        // structured `warn` (for fleet log aggregation) and a raw stderr line
        // (guaranteed visible: the default path must not become traceless just
        // because someone set `RUST_LOG=error`). This is legibility, not a gate —
        // audit-grade logging stays a future opt-in per the agentic-first law.
        tracing::warn!(
            target: "cosmix_comp::kms_live",
            "kms-live taking DRM master of {} ({}) on tty{} UNATTENDED — no operator confirmation requested; pass --kms-confirm to require the typed nonce",
            preliminary.canonical_device.display(),
            preliminary.request.connector,
            preliminary.vt,
        );
        eprintln!(
            "kms-live: taking DRM master of {} ({}) on tty{} UNATTENDED (no --kms-confirm)",
            preliminary.canonical_device.display(),
            preliminary.request.connector,
            preliminary.vt,
        );
        String::new()
    };
    let post_confirmation_state = platform.observe_vt(tty.as_fd());
    let post_confirmation_vt = validate_vt(post_confirmation_state)?;
    if post_confirmation_vt != preliminary.vt {
        return Err(KmsLiveRefusal::VtChangedSinceAuthorisation);
    }
    let refreshed_device = platform.observe_device(&request);
    // Decide over the SAME `request` that governed the prompt/read branch above,
    // rather than a re-parse of `argv` (which `authorise_observed` deliberately
    // no longer receives): the "was `--kms-confirm` requested?" answer must be
    // identical on both sides, or an attended prompt could be paired with a
    // skipped comparison (any typed input accepted).
    let decision = decide_request(
        request,
        &typed,
        &nonce,
        post_confirmation_state,
        build,
        &refreshed_device,
    )?;
    if decision.stable_device_path != preliminary.stable_device_path {
        return Err(KmsLiveRefusal::DeviceStableIdentityChanged);
    }
    // The old typed token carried the canonical path and so refused this drift
    // incidentally; with the token reduced to a freshness code, the continuity
    // must be checked outright. A stable parent path can re-resolve to a
    // different card between the two observations (a reprobe under the same
    // PCI device), and without this comparison that card would be adopted and
    // opened before the held incarnation could refuse it — after authority
    // had already changed hands.
    if decision.canonical_device != preliminary.canonical_device
        || decision.drm_device != preliminary.drm_device
    {
        return Err(KmsLiveRefusal::DeviceCanonicalIdentityChanged);
    }
    Ok(KmsLiveGrant {
        tty,
        canonical_device: decision.canonical_device,
        connector: decision.request.connector,
        presentation_backend: decision.request.presentation_backend,
        scene_mode: decision.request.scene_mode,
        output_scale: decision.request.output_scale,
        decoration: decision.request.decoration,
        authorised_vt: decision.vt,
        stable_device_path: decision.stable_device_path,
        drm_device: decision.drm_device,
        incarnation,
        platform,
    })
}

/// The only crate-visible route from an opaque grant to an opened DRM node.
///
/// D-2b replaces the deliberately unavailable inner operation. Callers cannot
/// inspect the grant, obtain a freshness token, choose another fd after
/// verification, or retain the verified wrapper outside this module-owned
/// operation boundary.
#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) fn execute_live(grant: KmsLiveGrant, bus_service: String) -> Result<(), KmsLiveError> {
    match prepare_live_operation(&grant, bus_service)? {
        Some(prepared) => operate_verified(grant, prepared),
        None => Ok(()),
    }
}

#[cfg(any(not(feature = "kms-live"), test))]
pub(crate) fn execute_live(grant: KmsLiveGrant, bus_service: String) -> Result<(), KmsLiveError> {
    let prepared = prepare_live_operation(&grant, bus_service)?;
    operate_verified(grant, prepared)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn prepare_live_operation(
    grant: &KmsLiveGrant,
    bus_service: String,
) -> Result<Option<PreparedLiveOperation>, KmsLiveError> {
    #[cfg(not(feature = "bus"))]
    drop(bus_service);
    let mut session = start_session_device_owner(grant.drm_device)?;
    let signals = LiveSignalWatcher::start(session.event_sender())?;
    let target_pairing = LiveTargetPairingLedger::default();
    let mut pump_preparation = super::render::LiveRenderPump::begin_prepare(
        grant.drm_device,
        grant.presentation_backend,
        session.event_sender(),
        target_pairing.clone(),
        grant.scene_mode,
        grant.decoration.clone(),
    )?;
    let started = Instant::now();
    let outcome = match supervise_live_pump_preparation(
        &mut session,
        &mut pump_preparation,
        || started.elapsed(),
        latched_live_signal,
    ) {
        Ok(outcome) => outcome,
        Err(error) => return Err(fail_live_pump_preparation(pump_preparation, error)),
    };
    if let LivePumpPreparationOutcome::End(end) = outcome {
        return match end {
            LiveSupervisionEnd::Revocation(revocation) => {
                log_live_authority_revoked(revocation, &grant.canonical_device);
                finish_revoked_live_pump_preparation(
                    session,
                    signals,
                    pump_preparation,
                    revocation,
                )?;
                Ok(None)
            }
            LiveSupervisionEnd::Signal(signal) => Err(fail_live_pump_preparation(
                pump_preparation,
                KmsLiveError::Signal(signal),
            )),
            LiveSupervisionEnd::VtSwitchRequested { vt, .. } => Err(fail_live_pump_preparation(
                pump_preparation,
                KmsLiveError::Setup(format!(
                    "VT switch {vt} was requested before the live protocol started"
                )),
            )),
            LiveSupervisionEnd::PauseRequested {
                generation,
                acknowledgement,
                ..
            } => {
                let aborted = pump_preparation.abort();
                let acknowledged = acknowledgement.acknowledge();
                let closed = session.close(SessionTeardown::Graceful);
                drop(signals);
                let outcome = Err(KmsLiveError::Setup(format!(
                    "external pause generation {generation} arrived before the persistent render island was ready"
                )));
                combine_live_results(
                    combine_live_results(outcome, aborted),
                    combine_live_results(
                        acknowledged.then_some(()).ok_or_else(|| {
                            KmsLiveError::Setup(
                                "external pause acknowledgement waiter vanished during startup"
                                    .into(),
                            )
                        }),
                        closed,
                    ),
                )?;
                unreachable!("the startup external pause outcome is terminal")
            }
        };
    }
    let prepared_pump = pump_preparation.finish();
    Ok(Some(PreparedLiveOperation {
        session: Some(session),
        pump: Some(prepared_pump.pump),
        output_selector: Some(prepared_pump.output_selector),
        protocol_wiring: Some(prepared_pump.protocol_wiring),
        signals: Some(signals),
        pending_vt_switch: None,
        pending_external_pause_ack: None,
        initial_render_commands: Vec::new(),
        topology_client: None,
        frame_clock: None,
        security_reporter: None,
        scene_feed: None,
        scene_mode: grant.scene_mode,
        decoration: grant.decoration.clone(),
        #[cfg(feature = "bus")]
        bus_service,
        output_scale: grant.output_scale,
        selected_output: None,
        resume_mode: None,
        lifecycle: None,
        active_fd_baseline: LiveActiveFdBaseline::default(),
        resume_cycle: 0,
        target_pairing,
        last_active_scanout: None,
    }))
}

#[cfg(all(feature = "kms-live", not(test)))]
fn finish_revoked_live_pump_preparation(
    session: SessionDeviceClient,
    signals: LiveSignalWatcher,
    preparation: super::render::LiveRenderPumpPreparation,
    revocation: LiveRevocation,
) -> Result<(), KmsLiveError> {
    let teardown = session_teardown_after(Some(revocation));
    let outcome = unresponsive_is_not_success(teardown);
    let outcome = combine_live_results(outcome, preparation.abort());
    let closed = session.close(teardown);
    drop(signals);
    combine_live_results(outcome, closed)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn fail_live_pump_preparation(
    preparation: super::render::LiveRenderPumpPreparation,
    error: KmsLiveError,
) -> KmsLiveError {
    combine_live_results(Err(error), preparation.abort())
        .expect_err("live pump preparation failure remains terminal after bounded cleanup")
}

#[cfg(all(feature = "kms-live", not(test)))]
fn start_session_device_owner(target_device: u64) -> Result<SessionDeviceClient, KmsLiveError> {
    let readiness_started = Instant::now();
    let (commands, command_source) = channel::channel();
    // Unbounded on purpose: it is the only channel the protocol thread may
    // signal on from inside a calloop callback, so `send` must never block.
    let (event_sender, events) = mpsc::channel();
    let fatal = LiveCoordinatorSender(event_sender);
    let thread_fatal = fatal.clone();
    let (ready_sender, ready_receiver) = startup_reply_channel();
    let thread = thread::Builder::new()
        .name("cosmix-kms-session".into())
        .spawn(move || {
            // First statement in the thread, so every path out of it — including
            // a panic before the loop is even reached — wakes the coordinator.
            let _exit = SessionThreadExitGuard(thread_fatal.clone());
            let built =
                build_session_device_owner(command_source, target_device, thread_fatal.clone());
            let (mut event_loop, mut state) = match built {
                Ok(built) => built,
                Err(error) => {
                    let _ = ready_sender.send(Err(error.to_string()));
                    return;
                }
            };
            // The seat name is read here and carried out with readiness because
            // `Session::seat` needs the session, and the session never leaves
            // this thread. libinput needs the name on the protocol thread to
            // call `udev_assign_seat`, so it travels as a `String` — which is
            // `Send` — rather than by a second round trip that would have to
            // happen while the protocol thread is being built.
            let seat = state
                .owner
                .as_ref()
                .map(|owner| owner.session.seat())
                .unwrap_or_default();
            if ready_sender.send(Ok(seat)).is_err() {
                return;
            }
            while !state.stop {
                if let Err(error) = dispatch_live_session_round(
                    &mut state,
                    |state| event_loop.dispatch(None, state),
                    |state| state.stop,
                    |state| {
                        // DRM first: an input open requires the DRM authority to
                        // already be `Open`, so performing input first would
                        // refuse an open that the same batch was about to make
                        // legitimate.
                        perform_pending_session_open(state);
                        perform_pending_input_open(state);
                    },
                ) {
                    tracing::error!(%error, "live session calloop stopped");
                    break;
                }
            }
            // `break` rather than `return`, and the acknowledgement sent from
            // here rather than from the handler, so that every way out of the
            // loop converges on one ordered teardown. Dropping the event loop
            // drops the libseat notifier, whose `Rc` is the last strong
            // reference to the seat, and only once that foreign close has
            // returned is the coordinator told it may `join`. A thread that
            // stops for some other reason sends nothing and is detached, which
            // is the honest answer: nobody here can prove the close finished.
            let acknowledgement = state.shutdown_ack.take();
            drop(state);
            let event_loop_drop_started = Instant::now();
            drop(event_loop);
            tracing::info!(
                elapsed_us = event_loop_drop_started.elapsed().as_micros(),
                acknowledgement_held = acknowledgement.is_some(),
                "live session event loop was destroyed before shutdown acknowledgement"
            );
            if let Some(HeldShutdownAck { reply, result }) = acknowledgement {
                let delivered = reply.send(result).is_ok();
                tracing::info!(
                    delivered,
                    "live session shutdown acknowledgement was sent after event-loop destruction"
                );
            }
        })
        .map_err(|error| KmsLiveError::Setup(format!("live session thread failed: {error}")))?;
    // Cold regime: this first readiness can itself activate seatd/logind, so it
    // deliberately does not use the tighter running-session command bound.
    let readiness =
        classify_startup_wait(ready_receiver.recv_timeout(INITIAL_SESSION_READINESS_TIMEOUT));
    tracing::info!(
        elapsed_us = readiness_started.elapsed().as_micros(),
        success = matches!(&readiness, StartupWait::Proceed(Ok(_))),
        "live session-thread readiness wait finished"
    );
    match readiness {
        StartupWait::Proceed(Ok(seat)) => Ok(SessionDeviceClient {
            commands,
            events,
            fatal,
            seat,
            deferred_events: Vec::new(),
            thread: Some(thread),
        }),
        StartupWait::Proceed(Err(error)) => {
            // `build_session_device_owner` drops every partially constructed
            // libseat/calloop value before returning this error. After sending
            // it, the thread has no foreign teardown left and only returns, so
            // this acknowledgement is sufficient to make joining safe.
            let _ = thread.join();
            Err(KmsLiveError::Setup(error))
        }
        StartupWait::TimeoutWithRevocation(_) => {
            tracing::error!(
                deadline_secs = INITIAL_SESSION_READINESS_TIMEOUT.as_secs(),
                "live session thread did not become ready; detaching it without waiting"
            );
            drop(thread);
            Err(KmsLiveError::Setup(format!(
                "live session readiness did not answer within {}s; abandoning the session thread",
                INITIAL_SESSION_READINESS_TIMEOUT.as_secs()
            )))
        }
        StartupWait::LostChannel => {
            // The sole sender disappearing proves that the thread stopped
            // reporting readiness, not that unwinding finished every foreign
            // destructor. Without the ordered shutdown acknowledgement there
            // is no safe basis for `join`.
            tracing::error!(
                "live session readiness channel was lost; detaching the thread without waiting"
            );
            drop(thread);
            Err(KmsLiveError::Setup(
                "live session readiness channel was lost during preparation".into(),
            ))
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn build_session_device_owner(
    command_source: channel::Channel<LiveSessionCommand>,
    target_device: u64,
    revocations: LiveCoordinatorSender,
) -> Result<(EventLoop<'static, LiveSessionState>, LiveSessionState), KmsLiveError> {
    let (session, notifier) = LibSeatSession::new_with_deferred_disable(EXTERNAL_PAUSE_ACK_TIMEOUT)
        .map_err(|error| {
            KmsLiveError::Setup(format!("failed to create libseat session: {error}"))
        })?;
    let event_loop = EventLoop::try_new()
        .map_err(|error| KmsLiveError::Setup(format!("live calloop creation failed: {error}")))?;
    event_loop
        .handle()
        .insert_source(
            notifier,
            |event, (), state: &mut LiveSessionState| match event {
                DeferredSessionEvent::PauseRequested { acknowledgement } => {
                    match state.authority.request_pause() {
                        Ok(LivePauseRequestDisposition::External { generation }) => {
                            let acknowledgement = ExternalPauseAcknowledgement::new(move || {
                                acknowledgement.acknowledge()
                            });
                            let _ =
                                state
                                    .revocations
                                    .0
                                    .send(LiveCoordinatorEvent::PauseRequested {
                                        generation,
                                        acknowledgement,
                                    });
                        }
                        Ok(
                            LivePauseRequestDisposition::SelfSwitch { .. }
                            | LivePauseRequestDisposition::Duplicate,
                        ) => {
                            let _ = acknowledgement.acknowledge();
                        }
                        Err(error) => {
                            tracing::error!(%error, "could not classify deferred session pause");
                            drop(acknowledgement);
                        }
                    }
                }
                DeferredSessionEvent::Paused { outcome } => {
                    let resumable = deferred_pause_is_resumable(state.authority, outcome);
                    if resumable && !outcome.resumable() {
                        tracing::warn!(
                            ?outcome,
                            "self-switch disable acknowledgement did not complete after local quiescence; \
                             keeping the disabled session resumable"
                        );
                    }
                    if let Some(completion) = state.authority.complete_pause(resumable) {
                        match completion.cause {
                            LivePauseCause::External => {
                                let _ =
                                    state
                                        .revocations
                                        .0
                                        .send(LiveCoordinatorEvent::SessionPaused {
                                            generation: completion.generation,
                                            resumable: completion.resumable,
                                        });
                                if completion.activate_pending {
                                    let _ = state.revocations.0.send(
                                        LiveCoordinatorEvent::SessionActivate {
                                            generation: completion.generation,
                                        },
                                    );
                                }
                            }
                            LivePauseCause::SelfSwitch if completion.resumable => {
                                let _ = state.revocations.0.send(
                                    LiveCoordinatorEvent::SessionPauseConfirmed {
                                        generation: completion.generation,
                                    },
                                );
                            }
                            LivePauseCause::SelfSwitch => {
                                let _ =
                                    state
                                        .revocations
                                        .0
                                        .send(LiveCoordinatorEvent::SessionPaused {
                                            generation: completion.generation,
                                            resumable: false,
                                        });
                            }
                        }
                    }
                }
                DeferredSessionEvent::ActivateSession => {
                    if let Some(generation) = state.authority.activate() {
                        let _ = state
                            .revocations
                            .0
                            .send(LiveCoordinatorEvent::SessionActivate { generation });
                    }
                }
            },
        )
        .map_err(|error| {
            KmsLiveError::Setup(format!("libseat notifier registration failed: {error}"))
        })?;
    let udev = UdevBackend::new(session.seat())
        .map_err(|error| KmsLiveError::Setup(format!("live udev monitor failed: {error}")))?;
    event_loop
        .handle()
        .insert_source(udev, |event, (), state: &mut LiveSessionState| {
            let device_id = match event {
                UdevEvent::Added { device_id, .. }
                | UdevEvent::Changed { device_id }
                | UdevEvent::Removed { device_id } => device_id,
            };
            if device_id == state.target_device {
                publish_live_revocation(state, LiveRevocation::TargetHotplug);
            }
        })
        .map_err(|error| KmsLiveError::Setup(format!("udev registration failed: {error}")))?;
    event_loop
        .handle()
        .insert_source(
            command_source,
            |event, (), state: &mut LiveSessionState| match event {
                channel::Event::Msg(LiveSessionCommand::Open { path, reply }) => {
                    let pending = PendingLiveOpen { path, reply };
                    if let Err(PendingLiveOpen { reply, .. }) =
                        queue_live_session_open(&mut state.pending_open, pending)
                    {
                        let _ = reply.send(Err(KmsLiveError::Setup(
                            "a libseat DRM open is already pending".into(),
                        )));
                    }
                }
                channel::Event::Msg(LiveSessionCommand::OpenInput { path, flags, reply }) => {
                    let pending = PendingInputOpen { path, flags, reply };
                    if let Err(PendingInputOpen { reply, .. }) =
                        queue_live_session_open(&mut state.pending_input_open, pending)
                    {
                        // libinput opens one device at a time and waits for
                        // each, so a second in flight means a caller this
                        // design does not have. Refusing is right; queueing
                        // would hide it.
                        let _ = reply.send(Err(InputOpenRefusal::SessionInactive.errno()));
                    }
                }
                channel::Event::Msg(LiveSessionCommand::CloseInput { fd, reply }) => {
                    let result = close_session_input_fd(state, fd);
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::Duplicate { reply }) => {
                    let result = state
                        .owner
                        .as_ref()
                        .and_then(|owner| owner.original.as_ref())
                        .ok_or_else(|| "libseat DRM device is not retained".to_string())
                        .and_then(|fd| {
                            fd.as_fd()
                                .try_clone_to_owned()
                                .map_err(|error| format!("DRM lease dup failed: {error}"))
                        });
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::CaptureScanout {
                    connector_id,
                    connector_identity,
                    lifecycle_generation,
                    observed_at,
                    old_output_target_existed,
                    expected_primary_plane_id,
                    reply,
                }) => {
                    let result = state
                        .owner
                        .as_ref()
                        .and_then(|owner| owner.original.as_ref())
                        .ok_or_else(|| "libseat DRM device is not retained".to_string())
                        .and_then(|fd| {
                            super::resume_scanout::capture(
                                fd.as_fd(),
                                connector_id,
                                &connector_identity,
                                lifecycle_generation,
                                observed_at,
                                old_output_target_existed,
                                expected_primary_plane_id,
                            )
                            .map_err(|error| error.to_string())
                        });
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::SwitchVt {
                    vt,
                    confirm_self_pause,
                    reply,
                }) => {
                    let result = if state.owner.is_none() {
                        Err("libseat session owner is unavailable".to_string())
                    } else {
                        let causal = if confirm_self_pause {
                            state
                                .authority
                                .submit_self_switch()
                                .map_err(|error| error.to_string())
                        } else {
                            Ok(())
                        };
                        causal.and_then(|()| {
                            state
                                .owner
                                .as_mut()
                                .expect("the session owner was checked immediately above")
                                .session
                                .change_vt(i32::from(vt))
                                .map_err(|error| error.to_string())
                        })
                    };
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::BeginSelfSwitch { generation, reply }) => {
                    let result = state.authority.begin_self_switch(generation);
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::CloseOriginal { reply }) => {
                    let authority_already_revoked =
                        session_authority_devices_are_revoked(state.authority);
                    let result = state
                        .owner
                        .as_mut()
                        .map(|owner| owner.close_original(authority_already_revoked))
                        .unwrap_or_else(|| Err("libseat session owner is unavailable".into()));
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::BeginResume { reply }) => {
                    let result = state
                        .authority
                        .begin_resume()
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::ReturnPaused {
                    generation,
                    cause,
                    reply,
                }) => {
                    let authority_already_revoked =
                        session_authority_devices_are_revoked(state.authority);
                    let close = state
                        .owner
                        .as_mut()
                        .map(|owner| owner.close_original(authority_already_revoked))
                        .unwrap_or_else(|| Err("libseat session owner is unavailable".into()));
                    let result = close.and_then(|()| {
                        state
                            .authority
                            .return_to_paused(generation, cause)
                            .map_err(|error| error.to_string())
                    });
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::FinishResume { generation, reply }) => {
                    let result = state
                        .authority
                        .finish_resume(generation)
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
                channel::Event::Msg(LiveSessionCommand::Shutdown { reply }) => {
                    refuse_pending_session_open(
                        state,
                        "live session shut down before the authority open",
                    );
                    refuse_pending_input_open(state, InputOpenRefusal::SessionInactive);
                    let authority_already_revoked =
                        session_authority_devices_are_revoked(state.authority);
                    let result = state
                        .owner
                        .as_mut()
                        .map(|owner| owner.close_original(authority_already_revoked))
                        .unwrap_or(Ok(()));
                    state.owner.take();
                    state.stop = true;
                    // Held, not sent. See `LiveSessionState::shutdown_ack`: the
                    // seat behind the session this handler just dropped is not
                    // closed until the event loop is.
                    state.shutdown_ack = Some(HeldShutdownAck { reply, result });
                }
                channel::Event::Closed => {
                    refuse_pending_session_open(
                        state,
                        "live session command channel closed before the authority open",
                    );
                    refuse_pending_input_open(state, InputOpenRefusal::SessionInactive);
                    state.owner.take();
                    state.stop = true;
                }
            },
        )
        .map_err(|error| {
            KmsLiveError::Setup(format!("session command registration failed: {error}"))
        })?;
    Ok((
        event_loop,
        LiveSessionState {
            target_device,
            authority: LiveSessionAuthority::initial(),
            owner: Some(SessionDeviceOwner {
                session,
                original: None,
            }),
            pending_open: None,
            pending_input_open: None,
            revocations,
            shutdown_ack: None,
            stop: false,
        },
    ))
}

#[cfg(any(not(feature = "kms-live"), test))]
fn prepare_live_operation(
    _grant: &KmsLiveGrant,
    _bus_service: String,
) -> Result<PreparedLiveOperation, KmsLiveError> {
    Ok(PreparedLiveOperation)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn operate_verified(
    grant: KmsLiveGrant,
    prepared: PreparedLiveOperation,
) -> Result<(), KmsLiveError> {
    validate_authorised_vt(&grant)?;
    act_live_operation(prepared, grant)
}

#[cfg(any(not(feature = "kms-live"), test))]
fn operate_verified(
    grant: KmsLiveGrant,
    prepared: PreparedLiveOperation,
) -> Result<(), KmsLiveError> {
    validate_authorised_vt(&grant)?;
    let _ = prepared;
    Err(KmsLiveRefusal::LiveBodyUnavailable.into())
}

#[cfg(test)]
fn operate_verified_with<P: LiveActPlatform>(
    platform: &mut P,
    grant: KmsLiveGrant,
) -> Result<(), KmsLiveError> {
    validate_authorised_vt(&grant)?;
    act_live_operation_with(platform, grant)
}

fn validate_authorised_vt(grant: &KmsLiveGrant) -> Result<(), KmsLiveRefusal> {
    let fresh_vt = validate_vt(grant.platform.observe_vt(grant.tty.as_fd()))?;
    if fresh_vt != grant.authorised_vt {
        return Err(KmsLiveRefusal::VtChangedSinceAuthorisation);
    }
    Ok(())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[cfg(any(all(feature = "kms-live", not(test)), test))]
/// A live atomic commit can truthfully report authority loss just before the
/// session callback publishes cancellation. Only external-pause reconciliation
/// or its bounded completed-update attribution window may contextualise that
/// report; all other render failure consumers stay strict.
fn atomic_commit_failure_is_pause_attributable(
    failure: &super::worker::KmsRenderWorkerFailure,
) -> Option<i32> {
    failure.failure.atomic_commit_authority_errno()
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct SubmitWatchdog {
    last_submitted_at: Duration,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl SubmitWatchdog {
    fn new(started_at: Duration) -> Self {
        Self {
            last_submitted_at: started_at,
        }
    }

    fn observe_submitted(&mut self, now: Duration) {
        self.last_submitted_at = now;
    }

    fn observe_cancelled_cycle(&mut self, now: Duration) {
        // Only the typed PresentationCancelled event reaches this method. It
        // is a lifecycle boundary, not evidence that a live output stopped
        // submitting, so it suspends the second independent kill clock. An
        // ordinary empty update never calls this method.
        self.last_submitted_at = now;
    }

    fn no_submit_timed_out(&self, now: Duration) -> bool {
        now.saturating_sub(self.last_submitted_at) >= NO_SUBMIT_TIMEOUT
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn observe_update_watchdog_evidence(
    policy: &mut SubmitWatchdog,
    events: &[KmsRenderFrameEvent],
    observed_at: Duration,
) -> Result<(), KmsLiveError> {
    if events
        .iter()
        .any(|event| matches!(event, KmsRenderFrameEvent::PresentationCancelled { .. }))
    {
        policy.observe_cancelled_cycle(observed_at);
        return Ok(());
    }
    if policy.no_submit_timed_out(observed_at) {
        return Err(no_submit_timeout_error());
    }
    Ok(())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct SubmittedFrameTelemetry {
    total: u64,
    interval_count: u64,
    interval_started_at: Duration,
    first_submitted: bool,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl SubmittedFrameTelemetry {
    fn new(started_at: Duration) -> Self {
        Self {
            total: 0,
            interval_count: 0,
            interval_started_at: started_at,
            first_submitted: false,
        }
    }

    fn observe(&mut self, now: Duration, generation: u64, key: &super::kms::OutputKey) {
        self.total = self.total.saturating_add(1);
        self.interval_count = self.interval_count.saturating_add(1);
        if !self.first_submitted {
            self.first_submitted = true;
            tracing::info!(
                generation,
                device = key.device,
                connector = key.connector_name,
                "live KMS first frame submitted"
            );
        }
        let interval = now.saturating_sub(self.interval_started_at);
        if interval >= Duration::from_secs(1) {
            tracing::info!(
                submitted_frames = self.interval_count,
                total_submitted_frames = self.total,
                interval_ms = interval.as_millis(),
                "live KMS submitted-frame telemetry"
            );
            self.interval_count = 0;
            self.interval_started_at = now;
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LiveCoordinatorMailbox {
    fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError>;
    fn wait_for_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError>;

    fn pause_reconciliation_cause(&self, requested: LivePauseCause) -> LivePauseCause {
        requested
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Debug)]
struct CollectedExternalPause {
    generation: u64,
    acknowledgement: ExternalPauseAcknowledgement,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct PauseCollectingMailbox<'a, M> {
    inner: &'a mut M,
    collected: &'a mut Option<CollectedExternalPause>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<'a, M> PauseCollectingMailbox<'a, M> {
    fn new(inner: &'a mut M, collected: &'a mut Option<CollectedExternalPause>) -> Self {
        Self { inner, collected }
    }

    fn collect(&mut self, event: LiveCoordinatorEvent) -> Option<LiveCoordinatorEvent> {
        match event {
            LiveCoordinatorEvent::PauseRequested {
                generation,
                acknowledgement,
            } if self.collected.is_none() => {
                *self.collected = Some(CollectedExternalPause {
                    generation,
                    acknowledgement,
                });
                None
            }
            event if self.collected.is_some() => {
                discard_stale_external_pause_chord(event, "self-switch external-pause takeover")
            }
            event => Some(event),
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<M: LiveCoordinatorMailbox> LiveCoordinatorMailbox for PauseCollectingMailbox<'_, M> {
    fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        loop {
            let Some(event) = self.inner.poll_event()? else {
                return Ok(None);
            };
            if let Some(event) = self.collect(event) {
                return Ok(Some(event));
            }
        }
    }

    fn wait_for_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        let started = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(event) = self.inner.wait_for_event_timeout(remaining)? else {
                return Ok(None);
            };
            if let Some(event) = self.collect(event) {
                return Ok(Some(event));
            }
        }
    }

    fn pause_reconciliation_cause(&self, requested: LivePauseCause) -> LivePauseCause {
        if self.collected.is_some() {
            LivePauseCause::External
        } else {
            requested
        }
    }
}

/// Mailbox view used once an external pause owns the lifecycle generation.
///
/// A later compositor chord cannot regain causality: device authority was
/// already revoked before the external pause callback, and submitting another
/// VT change would turn a resumable pause into a setup failure. Consume and log
/// those stale requests while preserving every other event in order.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct ExternalPauseMailbox<'a, M> {
    inner: &'a mut M,
    phase: &'static str,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<'a, M> ExternalPauseMailbox<'a, M> {
    fn new(inner: &'a mut M, phase: &'static str) -> Self {
        Self { inner, phase }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn discard_stale_external_pause_chord(
    event: LiveCoordinatorEvent,
    phase: &'static str,
) -> Option<LiveCoordinatorEvent> {
    match event {
        LiveCoordinatorEvent::VtSwitchRequested(vt) => {
            tracing::info!(
                vt,
                phase,
                "discarding VT-switch chord after an external pause won causality"
            );
            None
        }
        event => Some(event),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl<M: LiveCoordinatorMailbox> LiveCoordinatorMailbox for ExternalPauseMailbox<'_, M> {
    fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        loop {
            let Some(event) = self.inner.poll_event()? else {
                return Ok(None);
            };
            if let Some(event) = discard_stale_external_pause_chord(event, self.phase) {
                return Ok(Some(event));
            }
        }
    }

    fn wait_for_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        let started = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Ok(None);
            }
            let Some(event) = self.inner.wait_for_event_timeout(remaining)? else {
                return Ok(None);
            };
            if let Some(event) = discard_stale_external_pause_chord(event, self.phase) {
                return Ok(Some(event));
            }
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LivePumpPreparationControl {
    fn wait_slice(
        &mut self,
        timeout: Duration,
    ) -> Result<super::render::LiveRenderPreparationStatus, KmsLiveError>;
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Debug, Eq, PartialEq)]
enum LivePumpPreparationOutcome {
    Ready,
    End(LiveSupervisionEnd),
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LivePumpPreparationControl for super::render::LiveRenderPumpPreparation {
    fn wait_slice(
        &mut self,
        timeout: Duration,
    ) -> Result<super::render::LiveRenderPreparationStatus, KmsLiveError> {
        super::render::LiveRenderPumpPreparation::wait_slice(self, timeout)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn supervise_live_pump_preparation<M, P>(
    mailbox: &mut M,
    preparation: &mut P,
    mut now: impl FnMut() -> Duration,
    mut latched_signal: impl FnMut() -> Option<LiveSignal>,
) -> Result<LivePumpPreparationOutcome, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpPreparationControl,
{
    let started_at = now();
    loop {
        if let Some(end) =
            classify_pre_supervision_terminal(latched_signal(), None, "during render preparation")?
        {
            return Ok(LivePumpPreparationOutcome::End(end));
        }
        let event = mailbox.poll_event()?;
        if let Some(end) =
            classify_pre_supervision_terminal(latched_signal(), event, "during render preparation")?
        {
            return Ok(LivePumpPreparationOutcome::End(end));
        }
        let elapsed = now().saturating_sub(started_at);
        let remaining = LIVE_PUMP_PREPARATION_TIMEOUT.saturating_sub(elapsed);
        if remaining.is_zero() {
            return Err(KmsLiveError::Setup(format!(
                "live render pump preparation did not answer within {}s",
                LIVE_PUMP_PREPARATION_TIMEOUT.as_secs()
            )));
        }
        match preparation.wait_slice(remaining.min(LIVE_PREPARATION_MAILBOX_SLICE))? {
            super::render::LiveRenderPreparationStatus::Pending => {}
            super::render::LiveRenderPreparationStatus::Ready => {
                return Ok(LivePumpPreparationOutcome::Ready);
            }
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveCoordinatorMailbox for SessionDeviceClient {
    fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        SessionDeviceClient::poll_event(self)
    }

    fn wait_for_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
        SessionDeviceClient::wait_for_event_timeout(self, timeout)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LivePumpControl {
    fn request_registration(&mut self) -> Result<(), KmsLiveError>;
    fn request_update(&mut self) -> Result<(), KmsLiveError>;
    fn begin_stop(&mut self);
    fn nominal_refresh_interval(&self) -> Duration;
    fn begin_transition(&mut self, _commands: Vec<KmsRenderCommand>) -> Result<(), KmsLiveError> {
        Err(KmsLiveError::Setup(
            "pump transition control is unavailable".into(),
        ))
    }
    fn stage_resume_lease(
        &mut self,
        generation: u64,
        resume: super::render::StagedResumeLease,
    ) -> Result<(), KmsLiveError> {
        drop(resume);
        Err(KmsLiveError::Setup(format!(
            "pump resume lease staging is unavailable for generation {generation}"
        )))
    }
    fn transition_update(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        Err(KmsLiveError::Setup(format!(
            "pump transition update is unavailable for generation {generation}"
        )))
    }
    fn drain_scene(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        Err(KmsLiveError::Setup(format!(
            "pump scene drain is unavailable for generation {generation}"
        )))
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LivePumpControl for super::render::LiveRenderPump {
    fn request_registration(&mut self) -> Result<(), KmsLiveError> {
        self.poll_registration()
    }

    fn request_update(&mut self) -> Result<(), KmsLiveError> {
        self.update()
    }

    fn begin_stop(&mut self) {
        super::render::LiveRenderPump::begin_stop(self);
    }

    fn nominal_refresh_interval(&self) -> Duration {
        super::render::LiveRenderPump::nominal_refresh_interval(self)
    }

    fn begin_transition(&mut self, commands: Vec<KmsRenderCommand>) -> Result<(), KmsLiveError> {
        super::render::LiveRenderPump::begin_transition(self, commands)
    }

    fn stage_resume_lease(
        &mut self,
        generation: u64,
        resume: super::render::StagedResumeLease,
    ) -> Result<(), KmsLiveError> {
        super::render::LiveRenderPump::stage_resume_lease(self, generation, resume)
    }

    fn transition_update(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        super::render::LiveRenderPump::transition_update(self, generation)
    }

    fn drain_scene(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        super::render::LiveRenderPump::drain_scene(self, generation)
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Debug)]
enum LiveSupervisionEnd {
    Revocation(LiveRevocation),
    Signal(LiveSignal),
    VtSwitchRequested {
        vt: u8,
        outstanding_command: Option<OutstandingPumpCommand>,
    },
    PauseRequested {
        generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
        outstanding_command: Option<OutstandingPumpCommand>,
    },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl PartialEq for LiveSupervisionEnd {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Revocation(left), Self::Revocation(right)) => left == right,
            (Self::Signal(left), Self::Signal(right)) => left == right,
            (
                Self::VtSwitchRequested {
                    vt: left_vt,
                    outstanding_command: left_command,
                },
                Self::VtSwitchRequested {
                    vt: right_vt,
                    outstanding_command: right_command,
                },
            ) => left_vt == right_vt && left_command == right_command,
            (
                Self::PauseRequested {
                    generation: left_generation,
                    outstanding_command: left_command,
                    ..
                },
                Self::PauseRequested {
                    generation: right_generation,
                    outstanding_command: right_command,
                    ..
                },
            ) => left_generation == right_generation && left_command == right_command,
            _ => false,
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Eq for LiveSupervisionEnd {}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutstandingPumpCommand {
    Start,
    Registration,
    Update,
    DrainScene { generation: u64 },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Debug)]
enum ActiveLiveOperationEnd {
    Revocation {
        revocation: LiveRevocation,
        teardown: SessionTeardown,
    },
    VtSwitchRequested {
        vt: u8,
        outstanding_command: Option<OutstandingPumpCommand>,
    },
    PauseRequested {
        generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
        outstanding_command: Option<OutstandingPumpCommand>,
    },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl PartialEq for ActiveLiveOperationEnd {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Revocation {
                    revocation: left_revocation,
                    teardown: left_teardown,
                },
                Self::Revocation {
                    revocation: right_revocation,
                    teardown: right_teardown,
                },
            ) => left_revocation == right_revocation && left_teardown == right_teardown,
            (
                Self::VtSwitchRequested {
                    vt: left_vt,
                    outstanding_command: left_command,
                },
                Self::VtSwitchRequested {
                    vt: right_vt,
                    outstanding_command: right_command,
                },
            ) => left_vt == right_vt && left_command == right_command,
            (
                Self::PauseRequested {
                    generation: left_generation,
                    outstanding_command: left_command,
                    ..
                },
                Self::PauseRequested {
                    generation: right_generation,
                    outstanding_command: right_command,
                    ..
                },
            ) => left_generation == right_generation && left_command == right_command,
            _ => false,
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
impl Eq for ActiveLiveOperationEnd {}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
enum PumpWait {
    Reply(PumpReply),
    End(LiveSupervisionEnd),
}

#[cfg(test)]
fn supervise_live_render<M, P>(
    mailbox: &mut M,
    pump: &mut P,
    mut now: impl FnMut() -> Duration,
) -> Result<LiveSupervisionEnd, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpControl,
{
    let mut output_ready = |_| {};
    let mut pulse = || Ok(());
    let mut security_presented = |_, _, _| Ok(());
    let result = supervise_live_render_inner(
        mailbox,
        pump,
        &mut now,
        &mut output_ready,
        &mut pulse,
        &mut security_presented,
    );
    pump.begin_stop();
    result
}

#[cfg(test)]
fn supervise_active_live_operation<M, P>(
    mailbox: &mut M,
    pump: &mut P,
    now: impl FnMut() -> Duration,
) -> Result<ActiveLiveOperationEnd, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpControl,
{
    supervise_active_live_operation_after_output_ready(
        mailbox,
        pump,
        now,
        |_| {},
        || Ok(()),
        |_, _, _| Ok(()),
    )
}

/// The production active-operation arm: supervise the persistent render island,
/// report the first OutputReady boundary, turn terminal events into teardown,
/// and hand a compositor-requested switch to the pause coordinator without
/// stopping the pump. Kept shared with tests so both branches are exercised
/// without DRM access.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn supervise_active_live_operation_after_output_ready<M, P, R, F, S>(
    mailbox: &mut M,
    pump: &mut P,
    now: impl FnMut() -> Duration,
    mut output_ready: R,
    mut pulse: F,
    mut security_presented: S,
) -> Result<ActiveLiveOperationEnd, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpControl,
    R: FnMut(Duration),
    F: FnMut() -> Result<(), KmsLiveError>,
    S: FnMut(u64, u64, OutputKey) -> Result<(), KmsLiveError>,
{
    let mut now = now;
    let supervised = supervise_live_render_inner(
        mailbox,
        pump,
        &mut now,
        &mut output_ready,
        &mut pulse,
        &mut security_presented,
    );
    let end = match supervised {
        Ok(end) => end,
        Err(error) => {
            pump.begin_stop();
            return Err(error);
        }
    };
    match end {
        LiveSupervisionEnd::Revocation(revocation) => Ok(ActiveLiveOperationEnd::Revocation {
            revocation,
            teardown: session_teardown_after(Some(revocation)),
        })
        .inspect(|_| pump.begin_stop()),
        LiveSupervisionEnd::Signal(signal) => {
            pump.begin_stop();
            tracing::warn!(
                signal = signal.number(),
                "live KMS termination signal received"
            );
            Err(KmsLiveError::Signal(signal))
        }
        LiveSupervisionEnd::VtSwitchRequested {
            vt,
            outstanding_command,
        } => Ok(ActiveLiveOperationEnd::VtSwitchRequested {
            vt,
            outstanding_command,
        }),
        LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } => Ok(ActiveLiveOperationEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        }),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn supervise_live_render_inner<M, P, R, F, S>(
    mailbox: &mut M,
    pump: &mut P,
    now: &mut impl FnMut() -> Duration,
    output_ready: &mut R,
    pulse: &mut F,
    security_presented: &mut S,
) -> Result<LiveSupervisionEnd, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpControl,
    R: FnMut(Duration),
    F: FnMut() -> Result<(), KmsLiveError>,
    S: FnMut(u64, u64, OutputKey) -> Result<(), KmsLiveError>,
{
    let registration_started_at = now();
    let registration_deadline = registration_started_at.saturating_add(REGISTRATION_TIMEOUT);

    match wait_for_pump_reply(
        mailbox,
        registration_deadline,
        now,
        "start",
        OutstandingPumpCommand::Start,
    )? {
        PumpWait::End(end) => return Ok(end),
        PumpWait::Reply(PumpReply::Started(result)) => result?,
        PumpWait::Reply(reply) => return Err(unexpected_pump_reply("start", reply)),
    }

    let output_ready_at = loop {
        pump.request_registration()?;
        match wait_for_pump_reply(
            mailbox,
            registration_deadline,
            now,
            "registration",
            OutstandingPumpCommand::Registration,
        )? {
            PumpWait::End(end) => return Ok(end),
            PumpWait::Reply(PumpReply::Registration(Ok(LiveOutputRegistration::Ready))) => {
                let ready_at = now();
                tracing::info!(
                    elapsed_ms = ready_at.saturating_sub(registration_started_at).as_millis(),
                    "live KMS output ready"
                );
                output_ready(ready_at);
                break ready_at;
            }
            PumpWait::Reply(PumpReply::Registration(Ok(LiveOutputRegistration::Pending))) => {}
            PumpWait::Reply(PumpReply::Registration(Err(error))) => return Err(error),
            PumpWait::Reply(reply) => return Err(unexpected_pump_reply("registration", reply)),
        }
        if now() >= registration_deadline {
            return Err(registration_timeout_error());
        }
        if let Some(event) = mailbox.wait_for_event_timeout(
            pump.nominal_refresh_interval()
                .min(registration_deadline.saturating_sub(now())),
        )? {
            match event {
                LiveCoordinatorEvent::Revocation(revocation) => {
                    return Ok(LiveSupervisionEnd::Revocation(revocation));
                }
                LiveCoordinatorEvent::Signal(signal) => {
                    return Ok(LiveSupervisionEnd::Signal(signal));
                }
                LiveCoordinatorEvent::Pump(reply) => {
                    return Err(unexpected_pump_reply("registration backoff", reply));
                }
                LiveCoordinatorEvent::VtSwitchRequested(vt) => {
                    return Ok(LiveSupervisionEnd::VtSwitchRequested {
                        vt,
                        outstanding_command: None,
                    });
                }
                LiveCoordinatorEvent::PauseRequested {
                    generation,
                    acknowledgement,
                } => {
                    return Ok(LiveSupervisionEnd::PauseRequested {
                        generation,
                        acknowledgement,
                        outstanding_command: None,
                    });
                }
                LiveCoordinatorEvent::SessionPaused { .. }
                | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
                | LiveCoordinatorEvent::SessionActivate { .. } => {
                    return Err(unexpected_session_lifecycle("registration backoff"));
                }
            }
        }
    };

    let mut policy = SubmitWatchdog::new(output_ready_at);
    let mut telemetry = SubmittedFrameTelemetry::new(output_ready_at);
    loop {
        if let Some(end) = poll_terminal_event(mailbox)? {
            return Ok(end);
        }
        // The pump may remain inside one synchronous GPU update after DRM
        // master is revoked. Its session and protocol peers still latch the
        // event independently; the coordinator waits on this mailbox and its
        // own deadline, never on the update or the pump thread.
        pump.request_update()?;
        let submit_deadline = policy.last_submitted_at.saturating_add(NO_SUBMIT_TIMEOUT);
        let reply = match wait_for_pump_reply(
            mailbox,
            submit_deadline,
            now,
            "update",
            OutstandingPumpCommand::Update,
        )? {
            PumpWait::Reply(reply) => reply,
            PumpWait::End(end) => return Ok(end),
        };
        let observed_at = now();
        let events = match reply {
            PumpReply::Updated(result) => result?,
            reply => return Err(unexpected_pump_reply("update", reply)),
        };
        observe_update_watchdog_evidence(&mut policy, &events, observed_at)?;
        let mut submissions = 0_usize;
        for event in events {
            match event {
                KmsRenderFrameEvent::FrameSubmitted {
                    generation,
                    key,
                    security_epochs,
                    ..
                } => {
                    policy.observe_submitted(observed_at);
                    telemetry.observe(observed_at, generation, &key);
                    for presentation_epoch in security_epochs {
                        security_presented(presentation_epoch, generation, key.clone())?;
                    }
                    submissions = submissions.saturating_add(1);
                }
                KmsRenderFrameEvent::PresentationCancelled { .. } => {}
                KmsRenderFrameEvent::TerminalFailure(failure) => {
                    if atomic_commit_failure_is_pause_attributable(&failure).is_some() {
                        return await_external_pause_attribution_for_completed_update(
                            mailbox,
                            failure,
                            submit_deadline,
                            now,
                            "active atomic authority-failure attribution",
                        );
                    }
                    tracing::error!(
                        code = failure.failure.code,
                        detail = failure.failure.detail,
                        "live KMS render path failed"
                    );
                    return Err(terminal_render_failure_error(&failure));
                }
            }
        }
        pulse_client_frame_clock_for_update(submissions, pulse)?;
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[allow(clippy::too_many_arguments)]
fn supervise_resumed_live_render<M, P>(
    mailbox: &mut M,
    pump: &mut P,
    resumed: ResumedLiveOutput,
    scene_mode: LiveSceneMode,
    mut now: impl FnMut() -> Duration,
    mut flush_events: impl FnMut(Duration) -> Result<crate::protocol::EventFlushOutcome, KmsLiveError>,
    mut pulse: impl FnMut() -> Result<(), KmsLiveError>,
    mut security_presented: impl FnMut(u64, u64, OutputKey) -> Result<(), KmsLiveError>,
) -> Result<LiveSupervisionEnd, KmsLiveError>
where
    M: LiveCoordinatorMailbox,
    P: LivePumpControl,
{
    if scene_mode == LiveSceneMode::ClientContent {
        // The transition update which observed OutputReady drained stale scene
        // batches queued while paused. Draining cannot wake calloop, and a busy
        // client can refill both slots before our flush is handled. Alternate a
        // flush with a scene-only drain until the publisher confirms that nothing
        // remains compacted protocol-side. The OutputReady no-submit deadline
        // bounds sustained refill; only the following update is the first resumed
        // render and frame-clock pulse candidate. First-light leaves the scene
        // feed in WaylandRuntime, so it has nothing to drain and skips this stage.
        let submit_deadline = resumed.ready_at.saturating_add(NO_SUBMIT_TIMEOUT);
        loop {
            let observed_at = now();
            if observed_at >= submit_deadline {
                return Err(no_submit_timeout_error());
            }
            let timeout =
                LIVE_TOPOLOGY_ACK_TIMEOUT.min(submit_deadline.saturating_sub(observed_at));
            if flush_events(timeout)? == crate::protocol::EventFlushOutcome::Complete {
                if now() >= submit_deadline {
                    return Err(no_submit_timeout_error());
                }
                break;
            }
            pump.drain_scene(resumed.generation)?;
            let reply = match wait_for_pump_reply(
                mailbox,
                submit_deadline,
                &mut now,
                "resume scene drain",
                OutstandingPumpCommand::DrainScene {
                    generation: resumed.generation,
                },
            )? {
                PumpWait::Reply(reply) => reply,
                PumpWait::End(end) => return Ok(end),
            };
            match reply {
                PumpReply::SceneDrained { generation, result }
                    if generation == resumed.generation =>
                {
                    result?;
                }
                reply => return Err(unexpected_pump_reply("resume scene drain", reply)),
            }
        }
    }
    let mut policy = SubmitWatchdog::new(resumed.ready_at);
    let mut telemetry = SubmittedFrameTelemetry::new(resumed.ready_at);
    loop {
        if let Some(end) = poll_terminal_event(mailbox)? {
            return Ok(end);
        }
        pump.request_update()?;
        let submit_deadline = policy.last_submitted_at.saturating_add(NO_SUBMIT_TIMEOUT);
        let reply = match wait_for_pump_reply(
            mailbox,
            submit_deadline,
            &mut now,
            "update",
            OutstandingPumpCommand::Update,
        )? {
            PumpWait::Reply(reply) => reply,
            PumpWait::End(end) => return Ok(end),
        };
        let observed_at = now();
        let events = match reply {
            PumpReply::Updated(result) => result?,
            reply => return Err(unexpected_pump_reply("resumed update", reply)),
        };
        observe_update_watchdog_evidence(&mut policy, &events, observed_at)?;
        let mut submissions = 0_usize;
        for event in events {
            match event {
                KmsRenderFrameEvent::FrameSubmitted {
                    generation,
                    key,
                    security_epochs,
                    ..
                } => {
                    require_resumed_frame_generation(resumed.generation, generation)?;
                    policy.observe_submitted(observed_at);
                    telemetry.observe(observed_at, generation, &key);
                    for presentation_epoch in security_epochs {
                        security_presented(presentation_epoch, generation, key.clone())?;
                    }
                    submissions = submissions.saturating_add(1);
                }
                KmsRenderFrameEvent::PresentationCancelled { generation, .. } => {
                    require_resumed_frame_generation(resumed.generation, generation)?;
                }
                KmsRenderFrameEvent::TerminalFailure(failure) => {
                    if atomic_commit_failure_is_pause_attributable(&failure).is_some() {
                        return await_external_pause_attribution_for_completed_update(
                            mailbox,
                            failure,
                            submit_deadline,
                            &mut now,
                            "resumed atomic authority-failure attribution",
                        );
                    }
                    return Err(terminal_render_failure_error(&failure));
                }
            }
        }
        pulse_client_frame_clock_for_update(submissions, &mut pulse)?;
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn pulse_client_frame_clock_for_update(
    submissions: usize,
    pulse: &mut impl FnMut() -> Result<(), KmsLiveError>,
) -> Result<(), KmsLiveError> {
    if submissions > 0 {
        pulse()?;
    }
    Ok(())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn require_resumed_frame_generation(expected: u64, observed: u64) -> Result<(), KmsLiveError> {
    if observed == expected {
        Ok(())
    } else {
        Err(KmsLiveError::Setup(format!(
            "kms-live-stale-generation: resumed frame generation {observed} does not match output generation {expected}"
        )))
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn terminal_render_failure_error(failure: &super::worker::KmsRenderWorkerFailure) -> KmsLiveError {
    KmsLiveError::Setup(format!(
        "{}: {}",
        failure.failure.code, failure.failure.detail
    ))
}

/// A completed update carrying atomic EACCES/EPERM/ENODEV can beat the session
/// callback which proves that authority loss belongs to an external pause.
/// Keep the worker alive and wait only to the update's existing absolute
/// no-submit deadline. A matching pause contextualises the event through the
/// same reconciliation policy as an outstanding update; absence of that proof
/// preserves the original named terminal failure.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn await_external_pause_attribution_for_completed_update<M: LiveCoordinatorMailbox>(
    mailbox: &mut M,
    failure: super::worker::KmsRenderWorkerFailure,
    deadline: Duration,
    now: &mut impl FnMut() -> Duration,
    phase: &'static str,
) -> Result<LiveSupervisionEnd, KmsLiveError> {
    debug_assert!(atomic_commit_failure_is_pause_attributable(&failure).is_some());
    let terminal = || terminal_render_failure_error(&failure);
    let event = if let Some(event) = mailbox.poll_event()? {
        Some(event)
    } else {
        let observed_at = now();
        if observed_at >= deadline {
            return Err(terminal());
        }
        mailbox.wait_for_event_timeout(deadline.saturating_sub(observed_at))?
    };
    match event {
        Some(LiveCoordinatorEvent::PauseRequested {
            generation,
            acknowledgement,
        }) => {
            reconcile_pause_updated_frame_events(
                vec![KmsRenderFrameEvent::TerminalFailure(failure)],
                LivePauseCause::External,
            )?;
            Ok(LiveSupervisionEnd::PauseRequested {
                generation,
                acknowledgement,
                outstanding_command: None,
            })
        }
        Some(LiveCoordinatorEvent::Revocation(revocation)) => {
            Ok(LiveSupervisionEnd::Revocation(revocation))
        }
        Some(LiveCoordinatorEvent::Signal(signal)) => Ok(LiveSupervisionEnd::Signal(signal)),
        Some(LiveCoordinatorEvent::VtSwitchRequested(vt)) => {
            tracing::debug!(
                vt,
                phase,
                "self-switch cannot attribute an atomic authority failure before VT activation"
            );
            Err(terminal())
        }
        Some(LiveCoordinatorEvent::Pump(reply)) => Err(unexpected_pump_reply(
            "authority-failure attribution",
            reply,
        )),
        Some(
            LiveCoordinatorEvent::SessionPaused { .. }
            | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
            | LiveCoordinatorEvent::SessionActivate { .. },
        ) => Err(unexpected_session_lifecycle(phase)),
        None => Err(terminal()),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn wait_for_pump_reply<M: LiveCoordinatorMailbox>(
    mailbox: &mut M,
    deadline: Duration,
    now: &mut impl FnMut() -> Duration,
    phase: &'static str,
    outstanding_command: OutstandingPumpCommand,
) -> Result<PumpWait, KmsLiveError> {
    if let Some(event) = mailbox.poll_event()? {
        return classify_pump_wait_event(event, outstanding_command);
    }
    let observed_at = now();
    if observed_at >= deadline {
        return Err(if phase == "registration" || phase == "start" {
            registration_timeout_error()
        } else {
            no_submit_timeout_error()
        });
    }
    let event = mailbox.wait_for_event_timeout(deadline.saturating_sub(observed_at))?;
    match event {
        Some(event) => classify_pump_wait_event(event, outstanding_command),
        None => Err(if phase == "registration" || phase == "start" {
            registration_timeout_error()
        } else {
            no_submit_timeout_error()
        }),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn classify_pump_wait_event(
    event: LiveCoordinatorEvent,
    outstanding_command: OutstandingPumpCommand,
) -> Result<PumpWait, KmsLiveError> {
    match event {
        LiveCoordinatorEvent::Pump(reply) => Ok(PumpWait::Reply(reply)),
        LiveCoordinatorEvent::Revocation(revocation) => {
            Ok(PumpWait::End(LiveSupervisionEnd::Revocation(revocation)))
        }
        LiveCoordinatorEvent::Signal(signal) => {
            Ok(PumpWait::End(LiveSupervisionEnd::Signal(signal)))
        }
        LiveCoordinatorEvent::VtSwitchRequested(vt) => {
            Ok(PumpWait::End(LiveSupervisionEnd::VtSwitchRequested {
                vt,
                outstanding_command: Some(outstanding_command),
            }))
        }
        LiveCoordinatorEvent::PauseRequested {
            generation,
            acknowledgement,
        } => Ok(PumpWait::End(LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command: Some(outstanding_command),
        })),
        LiveCoordinatorEvent::SessionPaused { .. }
        | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
        | LiveCoordinatorEvent::SessionActivate { .. } => {
            Err(unexpected_session_lifecycle("pump wait"))
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn poll_terminal_event<M: LiveCoordinatorMailbox>(
    mailbox: &mut M,
) -> Result<Option<LiveSupervisionEnd>, KmsLiveError> {
    match mailbox.poll_event()? {
        Some(LiveCoordinatorEvent::Revocation(revocation)) => {
            Ok(Some(LiveSupervisionEnd::Revocation(revocation)))
        }
        Some(LiveCoordinatorEvent::Signal(signal)) => Ok(Some(LiveSupervisionEnd::Signal(signal))),
        Some(LiveCoordinatorEvent::Pump(reply)) => {
            Err(unexpected_pump_reply("before update", reply))
        }
        Some(LiveCoordinatorEvent::VtSwitchRequested(vt)) => {
            Ok(Some(LiveSupervisionEnd::VtSwitchRequested {
                vt,
                outstanding_command: None,
            }))
        }
        Some(LiveCoordinatorEvent::PauseRequested {
            generation,
            acknowledgement,
        }) => Ok(Some(LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command: None,
        })),
        Some(
            LiveCoordinatorEvent::SessionPaused { .. }
            | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
            | LiveCoordinatorEvent::SessionActivate { .. },
        ) => Err(unexpected_session_lifecycle("active render")),
        None => Ok(None),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn unexpected_session_lifecycle(phase: &'static str) -> KmsLiveError {
    KmsLiveError::Setup(format!(
        "live session sent an unexpected lifecycle confirmation during {phase}"
    ))
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
enum LiveTransitionOutcome {
    Suspended { generation: u64 },
    OutputReady { generation: u64 },
    OutputFailed { generation: u64, reason: String },
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResumedLiveOutput {
    ready_at: Duration,
    generation: u64,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn render_command_generation(command: &KmsRenderCommand) -> u64 {
    match command {
        KmsRenderCommand::Suspend { generation }
        | KmsRenderCommand::Resume { generation }
        | KmsRenderCommand::AddOutput { generation, .. }
        | KmsRenderCommand::ChangeOutput { generation, .. }
        | KmsRenderCommand::RemoveOutput { generation, .. } => *generation,
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn transition_resume_generation(commands: &[KmsRenderCommand]) -> Option<u64> {
    commands.iter().find_map(|command| match command {
        KmsRenderCommand::Resume { generation } => Some(*generation),
        _ => None,
    })
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn selected_output_for_adapter_start(
    selected_output: &Option<SelectedOutput>,
) -> Option<SelectedOutput> {
    selected_output.clone()
}

/// All AddOutput/ChangeOutput bindings in a drained transition, in order.
/// The live path emits exactly one today; collecting them all lets the
/// refresh below select by the READY generation instead of first-match,
/// so a future multi-output transition cannot install a foreign binding.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn resumed_selected_outputs(commands: &[KmsRenderCommand]) -> Vec<(u64, SelectedOutput)> {
    commands
        .iter()
        .filter_map(|command| match command {
            KmsRenderCommand::AddOutput { generation, output }
            | KmsRenderCommand::ChangeOutput { generation, output } => {
                Some((*generation, output.clone()))
            }
            _ => None,
        })
        .collect()
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn refresh_selected_output_after_resume(
    selected_output: &mut Option<SelectedOutput>,
    resumed_outputs: Vec<(u64, SelectedOutput)>,
    ready_generation: u64,
) -> Result<(), KmsLiveError> {
    let output = resumed_outputs
        .into_iter()
        .find(|(generation, _)| *generation == ready_generation)
        .map(|(_, output)| output)
        .ok_or_else(|| {
            KmsLiveError::Setup(format!(
                "resume topology emitted no selected-output command for ready generation {ready_generation}"
            ))
        })?;
    // A replaced connector is accepted here rather than refused: it can never
    // reach the retained-framebuffer scan-out path, because `retained_buffers`
    // in backend/render.rs is keyed by the NEW output key (see
    // `retained_buffers.remove(&output.key)` in `render_kms_frame`) and no
    // entry exists for a key that was never rendered. The lock-aware refusal in
    // `seamless_resume_is_eligible` is a second, independent gate.
    if let Some(retained) = selected_output.as_ref()
        && retained.key != output.key
    {
        tracing::info!(
            previous_output = retained.key.connector_name,
            replacement_output = output.key.connector_name,
            ready_generation,
            "session-lock-kms-output-replaced-blank"
        );
    }
    *selected_output = Some(output);
    Ok(())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn wait_for_transition_reply<M: LiveCoordinatorMailbox>(
    mailbox: &mut M,
    deadline: Duration,
    now: &mut impl FnMut() -> Duration,
    phase: &'static str,
) -> Result<PumpReply, KmsLiveError> {
    if let Some(event) = mailbox.poll_event()? {
        return classify_transition_wait_event(event, phase);
    }
    let observed_at = now();
    if observed_at >= deadline {
        return Err(KmsLiveError::Setup(format!(
            "live render {phase} reached its deadline"
        )));
    }
    match mailbox.wait_for_event_timeout(deadline.saturating_sub(observed_at))? {
        Some(event) => classify_transition_wait_event(event, phase),
        None => Err(KmsLiveError::Setup(format!(
            "live render {phase} reached its deadline"
        ))),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn classify_transition_wait_event(
    event: LiveCoordinatorEvent,
    phase: &'static str,
) -> Result<PumpReply, KmsLiveError> {
    match event {
        LiveCoordinatorEvent::Pump(reply) => Ok(reply),
        LiveCoordinatorEvent::Signal(signal) => Err(KmsLiveError::Signal(signal)),
        LiveCoordinatorEvent::Revocation(revocation) => {
            Err(KmsLiveError::AuthorityLost(revocation))
        }
        LiveCoordinatorEvent::VtSwitchRequested(vt) => Err(KmsLiveError::Setup(format!(
            "a second VT switch to {vt} was requested during {phase}"
        ))),
        LiveCoordinatorEvent::PauseRequested {
            generation,
            acknowledgement,
        } => Err(KmsLiveError::ExternalPauseRequested {
            generation,
            acknowledgement,
        }),
        LiveCoordinatorEvent::SessionPaused { .. }
        | LiveCoordinatorEvent::SessionPauseConfirmed { .. }
        | LiveCoordinatorEvent::SessionActivate { .. } => Err(unexpected_session_lifecycle(phase)),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn reconcile_outstanding_pump_command<M: LiveCoordinatorMailbox>(
    mailbox: &mut M,
    outstanding_command: OutstandingPumpCommand,
    pause_cause: LivePauseCause,
    deadline: Duration,
    now: &mut impl FnMut() -> Duration,
) -> Result<(), KmsLiveError> {
    let phase = "pre-transition outstanding-command reconcile";
    let reply = wait_for_transition_reply(mailbox, deadline, now, phase)?;
    let pause_cause = mailbox.pause_reconciliation_cause(pause_cause);
    match (outstanding_command, reply) {
        (OutstandingPumpCommand::Start, PumpReply::Started(result)) => result,
        (OutstandingPumpCommand::Registration, PumpReply::Registration(result)) => {
            result.map(|_status| ())
        }
        (OutstandingPumpCommand::Update, PumpReply::Updated(result)) => {
            reconcile_pause_updated_frame_events(result?, pause_cause)
        }
        (
            OutstandingPumpCommand::DrainScene {
                generation: expected,
            },
            PumpReply::SceneDrained { generation, result },
        ) if generation == expected => result,
        (_, reply) => Err(unexpected_pump_reply(phase, reply)),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn reconcile_pause_updated_frame_events(
    events: Vec<KmsRenderFrameEvent>,
    pause_cause: LivePauseCause,
) -> Result<(), KmsLiveError> {
    for event in events {
        match event {
            KmsRenderFrameEvent::FrameSubmitted { .. } => {}
            KmsRenderFrameEvent::PresentationCancelled { .. } => {}
            KmsRenderFrameEvent::TerminalFailure(failure)
                if pause_cause == LivePauseCause::External
                    && atomic_commit_failure_is_pause_attributable(&failure).is_some() =>
            {
                let errno = atomic_commit_failure_is_pause_attributable(&failure)
                    .expect("guard proved an authority-class atomic commit errno");
                tracing::debug!(
                    generation = failure.generation,
                    cause = ?pause_cause,
                    code = failure.failure.code,
                    errno,
                    detail = %failure.failure.detail,
                    "discarding atomic commit authority failure caused by established pause"
                );
            }
            KmsRenderFrameEvent::TerminalFailure(failure) => {
                return Err(KmsLiveError::TerminalFrame(format!(
                    "{}: {}",
                    failure.failure.code, failure.failure.detail
                )));
            }
        }
    }
    Ok(())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn drive_live_transition<M: LiveCoordinatorMailbox, P: LivePumpControl>(
    mailbox: &mut M,
    pump: &mut P,
    commands: Vec<KmsRenderCommand>,
    staged_lease: Option<(u64, super::render::StagedResumeLease)>,
    deadline: Duration,
    now: &mut impl FnMut() -> Duration,
) -> Result<LiveTransitionOutcome, KmsLiveError> {
    let transition_generation = commands
        .last()
        .map(render_command_generation)
        .ok_or_else(|| KmsLiveError::Setup("live topology emitted an empty transition".into()))?;
    if let Some((generation, resume)) = staged_lease {
        pump.stage_resume_lease(generation, resume)?;
        match wait_for_transition_reply(mailbox, deadline, now, "resume-lease staging")? {
            PumpReply::ResumeLeaseStaged {
                generation: replied,
                result,
            } if replied == generation => result?,
            reply => return Err(unexpected_pump_reply("resume-lease staging", reply)),
        }
    }
    pump.begin_transition(commands)?;
    match wait_for_transition_reply(mailbox, deadline, now, "transition begin")? {
        PumpReply::TransitionBegun { generation, result }
            if generation == transition_generation =>
        {
            result?
        }
        reply => return Err(unexpected_pump_reply("transition begin", reply)),
    }
    loop {
        pump.transition_update(transition_generation)?;
        let replies = match wait_for_transition_reply(mailbox, deadline, now, "transition update")?
        {
            PumpReply::TransitionUpdated { generation, result }
                if generation == transition_generation =>
            {
                result?
            }
            reply => return Err(unexpected_pump_reply("transition update", reply)),
        };
        for reply in replies {
            match reply {
                KmsRenderReply::Suspended { generation } => {
                    return Ok(LiveTransitionOutcome::Suspended { generation });
                }
                KmsRenderReply::OutputReady { generation, .. } => {
                    return Ok(LiveTransitionOutcome::OutputReady { generation });
                }
                KmsRenderReply::OutputFailed {
                    generation, reason, ..
                } => {
                    return Ok(LiveTransitionOutcome::OutputFailed { generation, reason });
                }
                KmsRenderReply::WorkerFailed { code, reason, .. } => {
                    return Err(KmsLiveError::Setup(format!(
                        "live render worker failed during transition: {code}: {reason}"
                    )));
                }
                KmsRenderReply::FrameSubmitted { .. } | KmsRenderReply::OutputRemoved { .. } => {}
            }
        }
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn registration_timeout_error() -> KmsLiveError {
    tracing::error!(
        timeout_ms = REGISTRATION_TIMEOUT.as_millis(),
        "live KMS output registration reached its deadline"
    );
    KmsLiveError::Setup("live KMS output registration did not complete within 30s".into())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn no_submit_timeout_error() -> KmsLiveError {
    tracing::error!(
        timeout_ms = NO_SUBMIT_TIMEOUT.as_millis(),
        "live KMS output reached the no-submit deadline"
    );
    KmsLiveError::Setup("live KMS output submitted no frame for 2s".into())
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn unexpected_pump_reply(phase: &'static str, reply: PumpReply) -> KmsLiveError {
    KmsLiveError::Setup(format!(
        "live render pump sent an unexpected reply during {phase}: {reply:?}"
    ))
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn remaining_resume_stage_timeout(
    deadline: Duration,
    observed_at: Duration,
    stage_bound: Duration,
) -> Result<Duration, KmsLiveError> {
    let remaining = deadline.saturating_sub(observed_at);
    if remaining.is_zero() {
        return Err(KmsLiveError::Setup(
            "live resume reached its 30s overall deadline".into(),
        ));
    }
    Ok(stage_bound.min(remaining))
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn run_resume_synchronous_stage<T>(
    deadline: Duration,
    observed_at: Duration,
    stage: &'static str,
    run: impl FnOnce() -> Result<T, KmsLiveError>,
) -> Result<T, KmsLiveError> {
    if observed_at >= deadline {
        return Err(KmsLiveError::Setup(format!(
            "live resume reached its 30s overall deadline before {stage}"
        )));
    }
    run()
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn log_live_authority_revoked(revocation: LiveRevocation, device: &Path) {
    tracing::warn!(
        ?revocation,
        device = %device.display(),
        "live KMS authority ended terminally; closing the session"
    );
}

/// The act-phase dependency boundary. Production binds these operations to
/// libseat, Vulkan, Smithay and the live render worker; tests bind them to
/// ordered inert tokens. The orchestration below is shared unchanged.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
trait LiveActPlatform {
    type Lease;
    type SelectedTarget;
    type Protocol;
    type Adapter;

    fn before_authority_open(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
        Ok(None)
    }

    /// Success means the session owner retained the original authority fd and
    /// returned a distinct verification duplicate as one indivisible step.
    fn open_authorised_device(&mut self, device_path: &Path) -> Result<OwnedFd, KmsLiveError>;
    fn duplicate_lease(&mut self) -> Result<Self::Lease, KmsLiveError>;
    fn discard_verification_fd(&mut self, verified: &mut VerifiedDrmFd);
    fn select_target(
        &mut self,
        verified: &VerifiedDrmFd,
    ) -> Result<Self::SelectedTarget, KmsLiveError>;
    fn start_protocol(
        &mut self,
        target: &Self::SelectedTarget,
    ) -> Result<Self::Protocol, KmsLiveError>;
    fn before_protocol_start(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
        Ok(None)
    }
    /// Refuse render startup when protocol construction already exposed a live
    /// operation ending condition.
    fn adapter_start_decision(&mut self) -> AdapterStartDecision;
    fn start_adapter(
        &mut self,
        lease: Self::Lease,
        _target: Self::SelectedTarget,
    ) -> Result<Self::Adapter, KmsLiveError>;
    /// Block until the live operation must end, and say how to take the session
    /// down.
    ///
    /// The teardown mode is a return value rather than platform state because it
    /// is a decision about the *reason* the wait ended, and the only place that
    /// reason exists is here.
    fn wait_for_revocation(
        &mut self,
        adapter: &mut Self::Adapter,
        verified: &VerifiedDrmFd,
        grant: &KmsLiveGrant,
    ) -> Result<SessionTeardown, KmsLiveError>;
    fn shutdown_adapter(&mut self, adapter: Self::Adapter) -> Result<(), KmsLiveError>;
    fn after_adapter_shutdown(&mut self) -> Result<(), KmsLiveError> {
        Ok(())
    }
    fn stop_protocol(&mut self, protocol: Self::Protocol);
    fn close_session(&mut self, teardown: SessionTeardown) -> Result<(), KmsLiveError>;
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn act_live_operation_with<P: LiveActPlatform>(
    platform: &mut P,
    grant: KmsLiveGrant,
) -> Result<(), KmsLiveError> {
    match platform.before_authority_open() {
        Ok(None) => {}
        Ok(Some(revocation)) => {
            log_live_authority_revoked(revocation, &grant.canonical_device);
            return combine_live_results(
                unresponsive_is_not_success(session_teardown_after(Some(revocation))),
                platform.close_session(session_teardown_after(Some(revocation))),
            );
        }
        Err(error) => {
            return combine_live_results(
                Err(error),
                platform.close_session(SessionTeardown::Graceful),
            );
        }
    }
    let opened = match platform.open_authorised_device(&grant.canonical_device) {
        Ok(opened) => opened,
        Err(error) => {
            // A timed-out session open has already queued `SessionUnresponsive`.
            // `close_session` re-decides against that notification and detaches;
            // an ordinary refusal leaves the graceful choice unchanged.
            return combine_live_results(
                Err(error),
                platform.close_session(SessionTeardown::Graceful),
            );
        }
    };
    act_live_operation_after_open(platform, grant, opened)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn act_live_operation_after_open<P: LiveActPlatform>(
    platform: &mut P,
    grant: KmsLiveGrant,
    opened: OwnedFd,
) -> Result<(), KmsLiveError> {
    let mut adapter = None;
    let mut protocol = None;
    let outcome = (|| {
        let opened_identity = grant.platform.observe_open_drm(opened.as_fd())?;
        if opened_identity.stable_device_path != grant.stable_device_path {
            return Err(KmsLiveRefusal::DeviceStableIdentityChanged.into());
        }
        grant
            .platform
            .validate_device_incarnation(&grant.incarnation, &opened_identity)?;
        let binding = grant
            .platform
            .scan_connector(opened.as_fd(), &opened_identity, &grant.connector)?
            .ok_or(KmsLiveRefusal::ConnectorNotPresent)?;
        validate_authorised_vt(&grant)?;
        let mut verified = VerifiedDrmFd {
            fd: Some(opened),
            connector_id: binding.connector_id,
            stable_device_path: opened_identity.stable_device_path,
            device_path: grant.canonical_device.clone(),
            device_id: opened_identity.rdev,
            connector_name: grant.connector.clone(),
        };
        let target = platform.select_target(&verified)?;
        let lease = platform.duplicate_lease()?;
        platform.discard_verification_fd(&mut verified);
        if let Some(revocation) = platform.before_protocol_start()? {
            log_live_authority_revoked(revocation, &verified.stable_device_path);
            return Ok(session_teardown_after(Some(revocation)));
        }
        protocol = Some(platform.start_protocol(&target)?);
        match platform.adapter_start_decision() {
            AdapterStartDecision::Start => {}
            AdapterStartDecision::EndAuthority(revocation) => {
                return Ok(session_teardown_after(Some(revocation)));
            }
            AdapterStartDecision::EndSignal(signal) => {
                return Err(KmsLiveError::Signal(signal));
            }
            AdapterStartDecision::EndVtSwitch(_) => {
                return Ok(SessionTeardown::Graceful);
            }
            AdapterStartDecision::RefuseInternal(revocation) => {
                let _teardown = session_teardown_after(Some(revocation));
                let failure = match revocation {
                    LiveRevocation::SessionUnresponsive => {
                        "the live session thread stopped answering while the protocol was starting"
                    }
                    LiveRevocation::SessionThreadStopped => {
                        "the live session thread stopped while the protocol was starting"
                    }
                    LiveRevocation::ProtocolThreadStopped => {
                        "the Wayland protocol thread stopped while the live protocol was starting"
                    }
                    LiveRevocation::SessionPause | LiveRevocation::TargetHotplug => {
                        unreachable!("authority revocations have their own startup decision")
                    }
                };
                return Err(KmsLiveError::Setup(failure.into()));
            }
        }
        adapter = Some(platform.start_adapter(lease, target)?);
        platform.wait_for_revocation(
            adapter
                .as_mut()
                .expect("the live adapter was installed immediately above"),
            &verified,
            &grant,
        )
    })();
    finish_live_operation(platform, adapter, protocol, outcome)
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn finish_live_operation<P: LiveActPlatform>(
    platform: &mut P,
    adapter: Option<P::Adapter>,
    protocol: Option<P::Protocol>,
    outcome: Result<SessionTeardown, KmsLiveError>,
) -> Result<(), KmsLiveError> {
    // An operation that failed before reaching the wait never learned of a
    // reason to detach, so it takes the ordinary route.
    let teardown = outcome
        .as_ref()
        .copied()
        .unwrap_or(SessionTeardown::Graceful);
    let outcome = outcome.and_then(unresponsive_is_not_success);
    // The single teardown funnel is deliberately ordered: DRM-backed render
    // resources, then the protocol frontend, then the libseat-owned original.
    let shutdown = adapter
        .map(|adapter| platform.shutdown_adapter(adapter))
        .unwrap_or(Ok(()));
    let after_shutdown = platform.after_adapter_shutdown();
    if let Some(protocol) = protocol {
        platform.stop_protocol(protocol);
    }
    let shutdown = combine_live_results(shutdown, after_shutdown);
    let outcome = combine_live_results(outcome, shutdown);
    combine_live_results(outcome, platform.close_session(teardown))
}

/// A live operation that ended because the session thread stopped answering did
/// not end cleanly, and must not report success.
///
/// A legacy pre-supervision authority loss or a hotplug is an ordinary end to
/// a live operation. Active external pauses instead pass through the deferred
/// pause/resume coordinator. A detach is not clean: descriptors and a libseat
/// session were leaked, and the process is exiting because a thread stopped
/// responding. Reporting that as `Ok` would hand a zero exit status to whatever
/// supervises the compositor.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn unresponsive_is_not_success(teardown: SessionTeardown) -> Result<(), KmsLiveError> {
    match teardown {
        SessionTeardown::Graceful => Ok(()),
        SessionTeardown::Detach => Err(KmsLiveError::Setup(
            "the live session thread stopped answering within its deadline; the compositor \
             terminated without it"
                .into(),
        )),
    }
}

/// A teardown that the revocations queue upgraded after the operation's result
/// was already decided must not report success either.
///
/// [`unresponsive_is_not_success`] runs inside `finish_live_operation`, against
/// the teardown the **coordinator** chose. `SessionDeviceClient::close` then
/// re-takes that decision against everything the channel still holds, and a
/// fatal notification queued behind the message the coordinator woke on turns a
/// `Graceful` into a `Detach` — after the result has been fixed. Without this,
/// the entire point of [`teardown_upgraded_by`] stops at the abandon: the
/// session thread and its libseat state are deliberately leaked and the process
/// still exits zero.
///
/// Only an *upgrade* carries the error. A teardown that was already `Detach`
/// when the coordinator chose it has been reported by
/// [`unresponsive_is_not_success`] already, and reporting it again would only
/// nest one description of a single event inside another.
#[cfg(any(feature = "kms-live", test))]
fn upgraded_detach_is_not_success(
    chosen: SessionTeardown,
    upgraded: SessionTeardown,
) -> Result<(), KmsLiveError> {
    match (chosen, upgraded) {
        (SessionTeardown::Graceful, SessionTeardown::Detach) => Err(KmsLiveError::Setup(
            "a fatal session notification was still queued when the live operation ended; the \
             session thread was abandoned rather than shut down, and whatever it holds is leaked"
                .into(),
        )),
        _ => Ok(()),
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
pub(crate) fn combine_live_results(
    outcome: Result<(), KmsLiveError>,
    cleanup: Result<(), KmsLiveError>,
) -> Result<(), KmsLiveError> {
    match (outcome, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(operation_error), Err(shutdown_error)) => Err(KmsLiveError::Setup(format!(
            "{operation_error}; additionally, cleanup failed: {shutdown_error}"
        ))),
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn act_live_operation(
    mut prepared: PreparedLiveOperation,
    grant: KmsLiveGrant,
) -> Result<(), KmsLiveError> {
    // Mailbox arbitration immediately precedes the authority-changing libseat
    // open. Speculative app, renderer and channel allocation remains confined
    // to `prepare_live_operation`, which has no DRM fd.
    act_live_operation_with(&mut prepared, grant)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn live_process_fd_count() -> Result<usize, KmsLiveError> {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .map_err(|error| KmsLiveError::Setup(format!("could not count /proc/self/fd: {error}")))
}

#[cfg(all(feature = "kms-live", not(test)))]
fn live_fd_telemetry(baseline: Option<usize>) -> (Option<usize>, Option<isize>) {
    match live_process_fd_count() {
        Ok(count) => (Some(count), live_fd_delta(Some(count), baseline)),
        Err(error) => {
            tracing::warn!(%error, "live resource fd telemetry was unavailable");
            (None, None)
        }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn live_pause_cause_name(cause: LivePauseCause) -> &'static str {
    match cause {
        LivePauseCause::External => "external",
        LivePauseCause::SelfSwitch => "self-switch",
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn resume_modeset_reason_name(reason: ResumeModesetReason) -> &'static str {
    match reason {
        ResumeModesetReason::GenerationMismatch => "generation-mismatch",
        ResumeModesetReason::InactiveCrtc => "inactive-crtc",
        ResumeModesetReason::RouteMismatch => "route-mismatch",
        ResumeModesetReason::ModeMismatch => "mode-mismatch",
        ResumeModesetReason::PlaneGeometryOrFormatMismatch => "plane-geometry-or-format-mismatch",
        ResumeModesetReason::NoUsableState => "no-usable-state",
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl PreparedLiveOperation {
    fn active_lifecycle_generation(&self) -> Option<u64> {
        match self.lifecycle.as_ref()?.state {
            LiveCoordinatorLifecycleState::Active { generation } => Some(generation),
            _ => None,
        }
    }

    fn capture_last_active_scanout(&mut self, observed_at: Duration) {
        let Some(generation) = self.active_lifecycle_generation() else {
            self.last_active_scanout = None;
            tracing::warn!("last-active scanout capture found no active lifecycle generation");
            return;
        };
        let Some(selected) = self.selected_output.as_ref() else {
            self.last_active_scanout = None;
            tracing::warn!("last-active scanout capture found no selected output");
            return;
        };
        let pairing = self.target_pairing.snapshot(generation);
        let result = self
            .session
            .as_ref()
            .expect("live session exists during last-active scanout capture")
            .capture_scanout(
                selected.connector_id,
                &selected.key.connector_name,
                generation,
                observed_at,
                pairing.created > pairing.released,
                None,
            );
        self.last_active_scanout = result
            .map_err(|error| {
                tracing::warn!(%error, generation, "last-active scanout capture was unavailable");
            })
            .ok();
    }

    fn log_resume_scanout_classification(&self, after: Option<&ResumeScanoutSnapshot>) {
        let before = self.last_active_scanout.as_ref();
        let classification = classify_resume_scanout(before, after);
        let (classification_name, reason) = match classification {
            ResumePresentationClassification::SeamlessPageFlip => ("seamless-page-flip", None),
            ResumePresentationClassification::ModesetRequired(reason) => {
                ("modeset-required", Some(resume_modeset_reason_name(reason)))
            }
        };
        let before_fb_id = before
            .and_then(|snapshot| snapshot.primary_plane.as_ref())
            .and_then(|plane| plane.fb.as_ref())
            .map(|framebuffer| framebuffer.fb_id);
        let after_fb_id = after
            .and_then(|snapshot| snapshot.primary_plane.as_ref())
            .and_then(|plane| plane.fb.as_ref())
            .map(|framebuffer| framebuffer.fb_id);
        let before_mode = before
            .and_then(|snapshot| snapshot.crtc.as_ref())
            .and_then(|crtc| crtc.mode.as_ref());
        let after_mode = after
            .and_then(|snapshot| snapshot.crtc.as_ref())
            .and_then(|crtc| crtc.mode.as_ref());
        let fb_id_same = matches!(
            (before_fb_id, after_fb_id),
            (Some(before), Some(after)) if before == after
        );
        tracing::info!(
            classification = classification_name,
            ?reason,
            ?before_fb_id,
            ?after_fb_id,
            fb_id_same,
            before_mode_width = before_mode.map(|mode| mode.size.0),
            before_mode_height = before_mode.map(|mode| mode.size.1),
            before_mode_refresh_millihz =
                before_mode.map(super::resume_scanout::ResumeModeTiming::refresh_millihz),
            after_mode_width = after_mode.map(|mode| mode.size.0),
            after_mode_height = after_mode.map(|mode| mode.size.1),
            after_mode_refresh_millihz =
                after_mode.map(super::resume_scanout::ResumeModeTiming::refresh_millihz),
            ?before_mode,
            ?after_mode,
            before_generation = before.map(|snapshot| snapshot.lifecycle_generation),
            after_generation = after.map(|snapshot| snapshot.lifecycle_generation),
            before_observed_ms = before.map(|snapshot| snapshot.observed_at.as_millis()),
            after_observed_ms = after.map(|snapshot| snapshot.observed_at.as_millis()),
            before_old_output_target_existed =
                before.map(|snapshot| snapshot.old_output_target_existed),
            after_old_output_target_existed =
                after.map(|snapshot| snapshot.old_output_target_existed),
            "kms-live resume scanout classification"
        );
    }

    fn log_output_ready_active_boundary(
        active_fd_baseline: &mut LiveActiveFdBaseline,
        generation: u64,
    ) -> LiveFdTelemetry {
        let (fd_count, _) = live_fd_telemetry(None);
        let telemetry = active_fd_baseline.observe_output_ready(fd_count);
        if telemetry.first_output_ready {
            tracing::info!(
                state = "Active",
                cycle = 0_u64,
                generation,
                ?telemetry.fd_count,
                ?telemetry.fd_delta,
                "kms-live lifecycle boundary"
            );
        }
        telemetry
    }

    fn log_paused_boundary(&self, generation: u64, cause: LivePauseCause) {
        let target_generation = generation.saturating_sub(1);
        let pairing = self.target_pairing.snapshot(target_generation);
        let retained = self.target_pairing.retained_snapshot(target_generation);
        let (fd_count, fd_delta) = live_fd_telemetry(self.active_fd_baseline.fd_count);
        tracing::info!(
            state = "Paused",
            cycle = self.resume_cycle,
            pause_cause = live_pause_cause_name(cause),
            generation,
            target_generation,
            ledger_created = pairing.created,
            ledger_released = pairing.released,
            ledger_paired = pairing.is_paired(),
            retained_ledger_created = retained.created,
            retained_ledger_released = retained.released,
            retained_ledger_outstanding = retained.outstanding(),
            retained_ledger_balanced = retained.is_balanced(),
            retained_ledger_pending_handoff = retained.pending_handoff(),
            retained_ledger_healthy = retained.is_healthy_while_paused(),
            ?fd_count,
            ?fd_delta,
            "kms-live lifecycle boundary"
        );
    }

    fn log_resumed_active_boundary(
        &mut self,
        paused_generation: u64,
        generation: u64,
        cause: LivePauseCause,
        resume_latency: Duration,
        retry_attempts_used: usize,
    ) {
        let telemetry =
            Self::log_output_ready_active_boundary(&mut self.active_fd_baseline, generation);
        let ledger_generation = paused_generation.saturating_sub(1);
        let cycle_pairing = self.target_pairing.snapshot(ledger_generation);
        let inactive_pairing = self.target_pairing.inactive_snapshot(generation);
        let cycle_retained = self.target_pairing.retained_snapshot(ledger_generation);
        let inactive_retained = self.target_pairing.inactive_retained_snapshot(generation);
        tracing::info!(
            state = "Active",
            cycle = self.resume_cycle,
            pause_cause = live_pause_cause_name(cause),
            paused_generation,
            generation,
            resume_latency_ms = resume_latency.as_millis(),
            retry_attempts_used,
            ledger_generation,
            ledger_created = cycle_pairing.created,
            ledger_released = cycle_pairing.released,
            ledger_paired = cycle_pairing.is_paired(),
            ledger_through_generation = generation.saturating_sub(1),
            inactive_ledger_created = inactive_pairing.created,
            inactive_ledger_released = inactive_pairing.released,
            inactive_ledger_paired = inactive_pairing.is_paired(),
            retained_ledger_created = cycle_retained.created,
            retained_ledger_released = cycle_retained.released,
            retained_ledger_outstanding = cycle_retained.outstanding(),
            retained_ledger_balanced = cycle_retained.is_balanced(),
            retained_ledger_pending_handoff = cycle_retained.pending_handoff(),
            retained_ledger_healthy = cycle_retained.is_healthy_while_active(),
            inactive_retained_ledger_created = inactive_retained.created,
            inactive_retained_ledger_released = inactive_retained.released,
            inactive_retained_ledger_balanced = inactive_retained.is_balanced(),
            inactive_retained_ledger_pending_handoffs = inactive_retained.pending_handoffs,
            inactive_retained_ledger_healthy = inactive_retained.is_healthy_while_active(),
            ?telemetry.fd_count,
            ?telemetry.fd_delta,
            "kms-live cycle telemetry at lifecycle boundary"
        );
    }

    fn cleanup_revoked_external_pause_devices(&mut self) -> Result<(), KmsLiveError> {
        // The external callback arrives after backend revocation. If protocol
        // startup got far enough to construct libinput, reconcile held state and
        // ask it to hand every device back before the acknowledgement. A pause
        // before protocol startup has no input source to reconcile.
        let input = self
            .topology_client
            .as_ref()
            .map(|topology| {
                topology
                    .reconcile_and_suspend_input(LIVE_INPUT_LIFECYCLE_TIMEOUT)
                    .map_err(KmsLiveError::Setup)
            })
            .unwrap_or(Ok(()));
        let original = self
            .session
            .as_ref()
            .expect("the live session exists until external pause acknowledgement")
            .close_original();
        combine_live_results(input, original)
    }

    fn arbitrate_coordinator_event(
        &mut self,
        phase: &'static str,
        may_acknowledge_immediately: bool,
    ) -> Result<Option<LiveRevocation>, KmsLiveError> {
        let event = self
            .session
            .as_mut()
            .expect("production preparation installs the session owner")
            .poll_event()?;
        match classify_pre_supervision_terminal(latched_live_signal(), event, phase)? {
            None => Ok(None),
            Some(LiveSupervisionEnd::Revocation(revocation)) => Ok(Some(revocation)),
            Some(LiveSupervisionEnd::Signal(signal)) => Err(KmsLiveError::Signal(signal)),
            Some(LiveSupervisionEnd::VtSwitchRequested { vt, .. }) => Err(KmsLiveError::Setup(
                format!("VT switch {vt} was requested before the live protocol started"),
            )),
            Some(LiveSupervisionEnd::PauseRequested {
                generation,
                acknowledgement,
                ..
            }) => {
                if may_acknowledge_immediately {
                    if acknowledgement.acknowledge() {
                        return Ok(Some(LiveRevocation::SessionPause));
                    }
                    return Err(KmsLiveError::Setup(format!(
                        "external pause generation {generation} lost its acknowledgement waiter before authority open"
                    )));
                }
                self.pending_external_pause_ack = Some(acknowledgement);
                tracing::info!(
                    generation,
                    "external pause arrived before live supervision; deferring its acknowledgement until startup cleanup"
                );
                Ok(Some(LiveRevocation::SessionPause))
            }
        }
    }

    fn topology_client(&self) -> Result<&crate::protocol::KmsTopologyClient, KmsLiveError> {
        self.topology_client
            .as_ref()
            .ok_or_else(|| KmsLiveError::Setup("live topology client is unavailable".into()))
    }

    fn submit_topology_transition(
        &self,
        event: super::kms::KmsTopologyLifecycleEvent,
        timeout: Duration,
    ) -> Result<Vec<KmsRenderCommand>, KmsLiveError> {
        let client = self.topology_client()?;
        client
            .submit_lifecycle(event, timeout)
            .map_err(KmsLiveError::Setup)?;
        client.drain_render_commands().map_err(KmsLiveError::Setup)
    }

    fn wait_for_self_pause_confirmation(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        let started = Instant::now();
        loop {
            let remaining = SELF_SWITCH_PAUSE_TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(missing_self_pause_confirmation(generation));
            }
            let event = self
                .session
                .as_mut()
                .expect("live session exists during self-switch")
                .wait_for_event_timeout(remaining)?;
            match event {
                Some(LiveCoordinatorEvent::SessionPauseConfirmed {
                    generation: confirmed,
                }) if confirmed == generation => return Ok(()),
                Some(LiveCoordinatorEvent::Signal(signal)) => {
                    return Err(KmsLiveError::Signal(signal));
                }
                Some(LiveCoordinatorEvent::Revocation(revocation)) => {
                    return Err(KmsLiveError::Setup(format!(
                        "live authority ended while awaiting self-pause confirmation: {revocation:?}"
                    )));
                }
                Some(event) => {
                    return Err(KmsLiveError::Setup(format!(
                        "unexpected coordinator event while awaiting self-pause confirmation: {event:?}"
                    )));
                }
                None => {}
            }
        }
    }

    fn wait_for_external_paused(&mut self, generation: u64) -> Result<(), KmsLiveError> {
        let started = Instant::now();
        loop {
            let remaining = EXTERNAL_PAUSED_TIMEOUT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(KmsLiveError::Setup(format!(
                    "external pause generation {generation} was acknowledged but libseat did not report Paused within {}s",
                    EXTERNAL_PAUSED_TIMEOUT.as_secs()
                )));
            }
            let event = self
                .session
                .as_mut()
                .expect("live session exists during external pause")
                .wait_for_event_timeout(remaining)?;
            let Some(event) = event.and_then(|event| {
                discard_stale_external_pause_chord(event, "external pause acknowledgement wait")
            }) else {
                continue;
            };
            match event {
                LiveCoordinatorEvent::SessionPaused {
                    generation: observed,
                    resumable: true,
                } if observed == generation => return Ok(()),
                LiveCoordinatorEvent::SessionPaused {
                    generation: observed,
                    resumable: false,
                } if observed == generation => {
                    return Err(KmsLiveError::Setup(format!(
                        "external pause generation {generation} became terminal while disabling the seat"
                    )));
                }
                LiveCoordinatorEvent::Signal(signal) => {
                    return Err(KmsLiveError::Signal(signal));
                }
                LiveCoordinatorEvent::Revocation(revocation) => {
                    return Err(KmsLiveError::Setup(format!(
                        "live authority ended while awaiting external pause completion: {revocation:?}"
                    )));
                }
                event => {
                    return Err(KmsLiveError::Setup(format!(
                        "unexpected coordinator event while awaiting external pause completion: {event:?}"
                    )));
                }
            }
        }
    }

    fn wait_for_activate(
        &mut self,
        generation: u64,
        cause: LivePauseCause,
    ) -> Result<(), KmsLiveError> {
        loop {
            let event = self
                .session
                .as_mut()
                .expect("live session exists while paused")
                .wait_for_event_timeout(Duration::from_secs(1))?;
            let Some(event) = event.and_then(|event| {
                if cause == LivePauseCause::External {
                    discard_stale_external_pause_chord(event, "externally paused activation wait")
                } else {
                    Some(event)
                }
            }) else {
                continue;
            };
            match event {
                LiveCoordinatorEvent::SessionActivate {
                    generation: activated,
                } if activated == generation => return Ok(()),
                LiveCoordinatorEvent::Signal(signal) => {
                    return Err(KmsLiveError::Signal(signal));
                }
                LiveCoordinatorEvent::Revocation(revocation) => {
                    return Err(KmsLiveError::Setup(format!(
                        "live authority ended while paused: {revocation:?}"
                    )));
                }
                LiveCoordinatorEvent::SessionPauseConfirmed {
                    generation: duplicate,
                } if duplicate == generation => {}
                LiveCoordinatorEvent::SessionPaused {
                    generation: duplicate,
                    resumable: true,
                } if duplicate == generation => {}
                event => {
                    return Err(KmsLiveError::Setup(format!(
                        "unexpected coordinator event while paused: {event:?}"
                    )));
                }
            }
        }
    }

    fn interruptible_resume_backoff(&mut self, duration: Duration) -> Result<(), KmsLiveError> {
        match self
            .session
            .as_mut()
            .expect("live session exists during resume backoff")
            .wait_for_event_timeout(duration)?
        {
            None => Ok(()),
            Some(LiveCoordinatorEvent::Signal(signal)) => Err(KmsLiveError::Signal(signal)),
            Some(LiveCoordinatorEvent::Revocation(revocation)) => Err(KmsLiveError::Setup(
                format!("live authority ended during resume backoff: {revocation:?}"),
            )),
            Some(event) => Err(KmsLiveError::Setup(format!(
                "unexpected coordinator event during resume backoff: {event:?}"
            ))),
        }
    }

    fn reopen_verified(
        &self,
        grant: &KmsLiveGrant,
        deadline: Duration,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<VerifiedDrmFd, ResumeAttemptFailure> {
        // Warm resume regime: the compositor and session thread are already
        // running, and the per-command cap remains inside the 30-second budget.
        let open_timeout =
            remaining_resume_stage_timeout(deadline, now(), RUNNING_SESSION_COMMAND_TIMEOUT)
                .map_err(ResumeAttemptFailure::Terminal)?;
        let opened = self
            .session
            .as_ref()
            .expect("live session exists during resume")
            .open_with_timeout(&grant.canonical_device, open_timeout)
            .map_err(|error| {
                if resume_authority_open_is_retryable(&error) {
                    ResumeAttemptFailure::Retry(error)
                } else {
                    ResumeAttemptFailure::Terminal(error)
                }
            })?;
        // These driver/library calls are synchronous and expose no internal
        // timeout. The overall budget therefore refuses each new stage at its
        // boundary; it cannot interrupt a call already inside the driver. A
        // probe that wedges there is covered by the compositor dead-man rescue,
        // not by this 30s budget.
        let opened_identity =
            run_resume_synchronous_stage(deadline, now(), "DRM identity observation", || {
                grant
                    .platform
                    .observe_open_drm(opened.as_fd())
                    .map_err(KmsLiveError::from)
            })
            .map_err(ResumeAttemptFailure::Terminal)?;
        if opened_identity.stable_device_path != grant.stable_device_path {
            return Err(ResumeAttemptFailure::Terminal(
                KmsLiveRefusal::DeviceStableIdentityChanged.into(),
            ));
        }
        run_resume_synchronous_stage(deadline, now(), "DRM incarnation validation", || {
            grant
                .platform
                .validate_device_incarnation(&grant.incarnation, &opened_identity)
                .map_err(KmsLiveError::from)
        })
        .map_err(ResumeAttemptFailure::Terminal)?;
        run_resume_synchronous_stage(deadline, now(), "VT validation", || {
            validate_authorised_vt(grant).map_err(KmsLiveError::from)
        })
        .map_err(ResumeAttemptFailure::Terminal)?;
        let master_state =
            run_resume_synchronous_stage(deadline, now(), "DRM master observation", || {
                Ok(borrowed_master_state(opened.as_fd()))
            })
            .map_err(ResumeAttemptFailure::Terminal)?;
        match master_state {
            Ok(DrmMasterState::RetainedImplicit) => {}
            Ok(DrmMasterState::NotMaster) | Err(_) => {
                return Err(ResumeAttemptFailure::Retry(KmsLiveError::Setup(
                    "kms-live-master-not-yet-observable".into(),
                )));
            }
        }
        let binding = run_resume_synchronous_stage(deadline, now(), "connector scan", || {
            grant
                .platform
                .scan_connector(opened.as_fd(), &opened_identity, &grant.connector)
                .map_err(KmsLiveError::from)
        })
        .map_err(ResumeAttemptFailure::Terminal)?
        .ok_or_else(|| {
            ResumeAttemptFailure::Terminal(KmsLiveRefusal::ConnectorNotPresent.into())
        })?;
        Ok(VerifiedDrmFd {
            fd: Some(opened),
            connector_id: binding.connector_id,
            stable_device_path: opened_identity.stable_device_path,
            device_path: grant.canonical_device.clone(),
            device_id: opened_identity.rdev,
            connector_name: grant.connector.clone(),
        })
    }

    fn return_failed_resume_to_paused(
        &mut self,
        adapter: &mut super::render::LiveRenderPump,
        paused: (u64, LivePauseCause),
        input_resumed: bool,
        render_resumed: bool,
        deadline: Duration,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<u64, KmsLiveError> {
        let (paused_generation, cause) = paused;
        if input_resumed {
            let timeout =
                remaining_resume_stage_timeout(deadline, now(), LIVE_INPUT_LIFECYCLE_TIMEOUT)?;
            self.topology_client()?
                .reconcile_and_suspend_input(timeout)
                .map_err(KmsLiveError::Setup)?;
        }
        let generation = if render_resumed {
            let timeout =
                remaining_resume_stage_timeout(deadline, now(), LIVE_TOPOLOGY_ACK_TIMEOUT)?;
            let commands = self.submit_topology_transition(
                super::kms::KmsTopologyLifecycleEvent::Pause,
                timeout,
            )?;
            let expected = commands
                .last()
                .map(render_command_generation)
                .ok_or_else(|| {
                    KmsLiveError::Setup("resume rollback emitted no suspend command".into())
                })?;
            match drive_live_transition(
                self.session
                    .as_mut()
                    .expect("live session exists during resume rollback"),
                adapter,
                commands,
                None,
                deadline,
                now,
            )? {
                LiveTransitionOutcome::Suspended { generation } if generation == expected => {
                    generation
                }
                outcome => {
                    return Err(KmsLiveError::Setup(format!(
                        "resume rollback did not return the render worker to Suspended: {outcome:?}"
                    )));
                }
            }
        } else {
            paused_generation
        };
        self.session
            .as_ref()
            .expect("live session exists during resume rollback")
            .return_paused(
                generation,
                cause,
                // Warm resume rollback; the session thread is still running.
                remaining_resume_stage_timeout(deadline, now(), RUNNING_SESSION_COMMAND_TIMEOUT)?,
            )?;
        self.lifecycle
            .as_mut()
            .expect("live lifecycle exists after adapter start")
            .apply(LiveCoordinatorLifecycleEvent::ResumeFailed { generation })
            .map_err(|error| KmsLiveError::Setup(error.detail))?;
        Ok(generation)
    }

    fn resume_after_activate(
        &mut self,
        adapter: &mut super::render::LiveRenderPump,
        grant: &KmsLiveGrant,
        mut paused_generation: u64,
        cause: LivePauseCause,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<ResumedLiveOutput, KmsLiveError> {
        let resume_started = now();
        let deadline = resume_started.saturating_add(LIVE_RESUME_TIMEOUT);
        let cycle_paused_generation = paused_generation;
        let required_mode = self
            .resume_mode
            .ok_or_else(|| KmsLiveError::Setup("prior live mode is unavailable".into()))?;
        let mut last_retry = None;
        for attempt in 0..3 {
            if now() >= deadline {
                return Err(KmsLiveError::Setup(
                    "live resume reached its 30s overall deadline".into(),
                ));
            }
            let session_generation = self
                .session
                .as_ref()
                .expect("live session exists during resume")
                // Warm resume regime: every command in this attempt runs after
                // the initial session-readiness boundary.
                .begin_resume(remaining_resume_stage_timeout(
                    deadline,
                    now(),
                    RUNNING_SESSION_COMMAND_TIMEOUT,
                )?)?;
            self.lifecycle
                .as_mut()
                .expect("live lifecycle exists after adapter start")
                .apply(LiveCoordinatorLifecycleEvent::BeginResume {
                    generation: session_generation,
                })
                .map_err(|error| KmsLiveError::Setup(error.detail))?;

            let mut input_resumed = false;
            let mut render_resumed = false;
            let mut scanout_after = None;
            let attempt_result = (|| -> Result<ResumedLiveOutput, ResumeAttemptFailure> {
                let mut reopened = self.reopen_verified(grant, deadline, now)?;
                let expected_primary_plane_id = self
                    .last_active_scanout
                    .as_ref()
                    .and_then(|snapshot| snapshot.primary_plane.as_ref())
                    .map(|plane| plane.id);
                let old_target_generation = cycle_paused_generation.saturating_sub(1);
                let old_target_pairing = self.target_pairing.snapshot(old_target_generation);
                scanout_after = super::resume_scanout::capture(
                    reopened
                        .fd
                        .as_ref()
                        .expect("fresh verification fd exists during resume scanout capture")
                        .as_fd(),
                    reopened.connector_id,
                    &reopened.connector_name,
                    session_generation,
                    now(),
                    old_target_pairing.created > old_target_pairing.released,
                    expected_primary_plane_id,
                )
                .map_err(|error| {
                    tracing::warn!(
                        %error,
                        generation = session_generation,
                        "master-visible resume scanout capture was unavailable"
                    );
                })
                .ok();
                let input_timeout =
                    remaining_resume_stage_timeout(deadline, now(), LIVE_INPUT_LIFECYCLE_TIMEOUT)
                        .map_err(ResumeAttemptFailure::Terminal)?;
                self.topology_client()?
                    .resume_input(input_timeout)
                    .map_err(|error| ResumeAttemptFailure::Terminal(KmsLiveError::Setup(error)))?;
                input_resumed = true;
                let lock_query_timeout =
                    remaining_resume_stage_timeout(deadline, now(), LIVE_TOPOLOGY_ACK_TIMEOUT)
                        .map_err(ResumeAttemptFailure::Terminal)?;
                let lock_active = self
                    .topology_client()?
                    .session_lock_active(lock_query_timeout)
                    .map_err(|error| ResumeAttemptFailure::Terminal(KmsLiveError::Setup(error)))?;
                let verification_fd = reopened
                    .fd
                    .as_ref()
                    .expect("fresh verification fd exists during resume selection");
                let target = select_live_target(
                    self.output_selector
                        .as_ref()
                        .expect("output selector persists across pause"),
                    verification_fd.as_fd(),
                    &reopened,
                    self.output_scale,
                    Some(required_mode),
                    Some(ResumeSynchronousBudget { deadline, now }),
                )
                .map_err(ResumeAttemptFailure::Terminal)?;
                let lease = self
                    .session
                    .as_ref()
                    .expect("live session exists during resume")
                    .duplicate_lease_with_timeout(
                        remaining_resume_stage_timeout(
                            deadline,
                            now(),
                            RUNNING_SESSION_COMMAND_TIMEOUT,
                        )
                        .map_err(ResumeAttemptFailure::Terminal)?,
                    )
                    .map_err(ResumeAttemptFailure::Terminal)?;
                drop(reopened.fd.take());
                let topology_timeout =
                    remaining_resume_stage_timeout(deadline, now(), LIVE_TOPOLOGY_ACK_TIMEOUT)
                        .map_err(ResumeAttemptFailure::Terminal)?;
                self.topology_client()?
                    .submit_lifecycle(
                        super::kms::KmsTopologyLifecycleEvent::Resume(target.topology),
                        topology_timeout,
                    )
                    .map_err(|error| ResumeAttemptFailure::Terminal(KmsLiveError::Setup(error)))?;
                let commands = self
                    .topology_client()?
                    .drain_render_commands()
                    .map_err(|error| ResumeAttemptFailure::Terminal(KmsLiveError::Setup(error)))?;
                let resume_generation =
                    transition_resume_generation(&commands).ok_or_else(|| {
                        ResumeAttemptFailure::Terminal(KmsLiveError::Setup(
                            "resume topology emitted no Resume command".into(),
                        ))
                    })?;
                if resume_generation != session_generation {
                    return Err(ResumeAttemptFailure::Terminal(KmsLiveError::Setup(
                        format!(
                            "kms-live-stale-generation: session resume {session_generation} does not match topology {resume_generation}"
                        ),
                    )));
                }
                let resumed_outputs = resumed_selected_outputs(&commands);
                let seamless_budget =
                    remaining_resume_stage_timeout(deadline, now(), LIVE_RESUME_TIMEOUT)
                        .map_err(ResumeAttemptFailure::Terminal)?;
                let staged_resume = super::render::StagedResumeLease {
                    lease,
                    presentation: super::render::ResumePresentationPlan {
                        classification: classify_resume_scanout(
                            self.last_active_scanout.as_ref(),
                            scanout_after.as_ref(),
                        ),
                        deadline: super::render::PresentDeadline::bounded(
                            Instant::now() + seamless_budget,
                        ),
                        lock_active,
                    },
                };
                render_resumed = true;
                match drive_live_transition(
                    self.session
                        .as_mut()
                        .expect("live session exists during render resume"),
                    adapter,
                    commands,
                    Some((resume_generation, staged_resume)),
                    deadline,
                    now,
                )
                .map_err(ResumeAttemptFailure::Terminal)?
                {
                    LiveTransitionOutcome::OutputReady { generation } => {
                        let ready_at = now();
                        self.session
                            .as_ref()
                            .expect("live session exists while resume completes")
                            .finish_resume(
                                generation,
                                remaining_resume_stage_timeout(
                                    deadline,
                                    now(),
                                    RUNNING_SESSION_COMMAND_TIMEOUT,
                                )
                                .map_err(ResumeAttemptFailure::Terminal)?,
                            )
                            .map_err(ResumeAttemptFailure::Terminal)?;
                        self.lifecycle
                            .as_mut()
                            .expect("live lifecycle exists during resume")
                            .apply(LiveCoordinatorLifecycleEvent::OutputReady {
                                generation,
                                observed_at: ready_at,
                            })
                            .map_err(|error| {
                                ResumeAttemptFailure::Terminal(KmsLiveError::Setup(error.detail))
                            })?;
                        refresh_selected_output_after_resume(
                            &mut self.selected_output,
                            resumed_outputs,
                            generation,
                        )
                        .map_err(ResumeAttemptFailure::Terminal)?;
                        Ok(ResumedLiveOutput {
                            ready_at,
                            generation,
                        })
                    }
                    LiveTransitionOutcome::OutputFailed { reason, .. } => {
                        Err(ResumeAttemptFailure::Terminal(KmsLiveError::Setup(reason)))
                    }
                    outcome => Err(ResumeAttemptFailure::Terminal(KmsLiveError::Setup(
                        format!("resume transition ended unexpectedly: {outcome:?}"),
                    ))),
                }
            })();

            match attempt_result {
                Ok(resumed) => {
                    self.log_resume_scanout_classification(scanout_after.as_ref());
                    self.log_resumed_active_boundary(
                        cycle_paused_generation,
                        resumed.generation,
                        cause,
                        resumed.ready_at.saturating_sub(resume_started),
                        attempt,
                    );
                    return Ok(resumed);
                }
                Err(ResumeAttemptFailure::Terminal(error)) => return Err(error),
                Err(ResumeAttemptFailure::Retry(error)) => {
                    last_retry = Some(error);
                    paused_generation = self.return_failed_resume_to_paused(
                        adapter,
                        (paused_generation, cause),
                        input_resumed,
                        render_resumed,
                        deadline,
                        now,
                    )?;
                    self.log_paused_boundary(paused_generation, cause);
                    if let Some(backoff) = LIVE_RESUME_BACKOFFS.get(attempt).copied() {
                        let remaining = deadline.saturating_sub(now());
                        if remaining.is_zero() {
                            break;
                        }
                        self.interruptible_resume_backoff(backoff.min(remaining))?;
                    }
                }
            }
        }
        Err(last_retry
            .unwrap_or_else(|| KmsLiveError::Setup("live resume exhausted three attempts".into())))
    }

    fn external_pause_and_resume(
        &mut self,
        adapter: &mut super::render::LiveRenderPump,
        grant: &KmsLiveGrant,
        requested_generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
        outstanding_command: Option<OutstandingPumpCommand>,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<ResumedLiveOutput, KmsLiveError> {
        // Ordering is pinned by the live `_bin/desk_atomic_gate.mix`: keep
        // cancellation ahead of capture and outstanding-command reconciliation.
        cancel_active_presentation_for_pause(
            self.lifecycle
                .as_ref()
                .expect("live lifecycle exists before external-pause cancellation"),
            "external-pause",
            |generation| adapter.cancel_generation_presentations(generation),
        )?;
        self.capture_last_active_scanout(now());
        let suspend_deadline = now().saturating_add(LIVE_RESUME_TIMEOUT);
        let suspended = (|| -> Result<u64, KmsLiveError> {
            if let Some(outstanding_command) = outstanding_command {
                let mut mailbox = ExternalPauseMailbox::new(
                    self.session
                        .as_mut()
                        .expect("live session exists during command reconciliation"),
                    "external pause command reconciliation",
                );
                reconcile_outstanding_pump_command(
                    &mut mailbox,
                    outstanding_command,
                    LivePauseCause::External,
                    suspend_deadline,
                    now,
                )?;
            }
            let commands = self.submit_topology_transition(
                super::kms::KmsTopologyLifecycleEvent::Pause,
                LIVE_TOPOLOGY_ACK_TIMEOUT,
            )?;
            let generation = commands
                .last()
                .map(render_command_generation)
                .ok_or_else(|| {
                    KmsLiveError::Setup("external pause topology emitted no Suspend command".into())
                })?;
            if generation != requested_generation {
                return Err(KmsLiveError::Setup(format!(
                    "kms-live-stale-generation: external pause request {requested_generation} does not match topology {generation}"
                )));
            }
            self.lifecycle
                .as_mut()
                .expect("live lifecycle exists after adapter start")
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation })
                .map_err(|error| KmsLiveError::Setup(error.detail))?;
            let transition = {
                let mut mailbox = ExternalPauseMailbox::new(
                    self.session
                        .as_mut()
                        .expect("live session exists during external render suspend"),
                    "external render suspend",
                );
                drive_live_transition(&mut mailbox, adapter, commands, None, suspend_deadline, now)?
            };
            match transition {
                LiveTransitionOutcome::Suspended {
                    generation: suspended,
                } if suspended == generation => {}
                outcome => {
                    return Err(KmsLiveError::Setup(format!(
                        "external render suspend ended unexpectedly: {outcome:?}"
                    )));
                }
            }
            self.lifecycle
                .as_mut()
                .expect("live lifecycle exists after adapter start")
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation })
                .map_err(|error| KmsLiveError::Setup(error.detail))?;
            self.topology_client()?
                .reconcile_and_suspend_input(LIVE_INPUT_LIFECYCLE_TIMEOUT)
                .map_err(KmsLiveError::Setup)?;
            // Ordering is pinned by the live `_bin/desk_atomic_gate.mix`:
            // atomic target release must precede closing original authority.
            self.session
                .as_ref()
                .expect("live session exists during external pause")
                .close_original()?;
            Ok(generation)
        })();
        let generation = match suspended {
            Ok(generation) => generation,
            Err(error) => {
                adapter.begin_stop();
                self.pending_external_pause_ack = Some(acknowledgement);
                return Err(error);
            }
        };
        self.finish_external_pause_and_resume(adapter, grant, generation, acknowledgement, now)
    }

    fn finish_external_pause_and_resume(
        &mut self,
        adapter: &mut super::render::LiveRenderPump,
        grant: &KmsLiveGrant,
        generation: u64,
        acknowledgement: ExternalPauseAcknowledgement,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<ResumedLiveOutput, KmsLiveError> {
        if !acknowledgement.acknowledge() {
            adapter.begin_stop();
            return Err(KmsLiveError::Setup(format!(
                "external pause generation {generation} lost its acknowledgement waiter"
            )));
        }
        self.wait_for_external_paused(generation)?;
        self.resume_cycle = self.resume_cycle.saturating_add(1);
        self.log_paused_boundary(generation, LivePauseCause::External);
        self.wait_for_activate(generation, LivePauseCause::External)?;
        self.resume_after_activate(adapter, grant, generation, LivePauseCause::External, now)
    }

    fn self_switch_and_resume(
        &mut self,
        adapter: &mut super::render::LiveRenderPump,
        grant: &KmsLiveGrant,
        vt: u8,
        outstanding_command: Option<OutstandingPumpCommand>,
        now: &mut impl FnMut() -> Duration,
    ) -> Result<ResumedLiveOutput, KmsLiveError> {
        // Ordering is pinned by the live `_bin/desk_atomic_gate.mix`: keep
        // cancellation ahead of begin_self_switch and VT_ACTIVATE submission.
        cancel_active_presentation_for_pause(
            self.lifecycle
                .as_ref()
                .expect("live lifecycle exists before self-switch cancellation"),
            "self-switch",
            |generation| adapter.cancel_generation_presentations(generation),
        )?;
        self.capture_last_active_scanout(now());
        let suspend_deadline = now().saturating_add(LIVE_RESUME_TIMEOUT);
        let mut external_pause = None;
        let prepared = (|| -> Result<u64, KmsLiveError> {
            if let Some(outstanding_command) = outstanding_command {
                let mut mailbox = PauseCollectingMailbox::new(
                    self.session
                        .as_mut()
                        .expect("live session exists during command reconciliation"),
                    &mut external_pause,
                );
                reconcile_outstanding_pump_command(
                    &mut mailbox,
                    outstanding_command,
                    LivePauseCause::SelfSwitch,
                    suspend_deadline,
                    now,
                )?;
            }
            let commands = self.submit_topology_transition(
                super::kms::KmsTopologyLifecycleEvent::Pause,
                LIVE_TOPOLOGY_ACK_TIMEOUT,
            )?;
            let generation = commands
                .last()
                .map(render_command_generation)
                .ok_or_else(|| {
                    KmsLiveError::Setup("pause topology emitted no Suspend command".into())
                })?;
            if let Some(pause) = external_pause.as_ref()
                && pause.generation != generation
            {
                return Err(KmsLiveError::Setup(format!(
                    "kms-live-stale-generation: racing external pause {} does not match self-switch topology {generation}",
                    pause.generation
                )));
            }
            self.lifecycle
                .as_mut()
                .expect("live lifecycle exists after adapter start")
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation })
                .map_err(|error| KmsLiveError::Setup(error.detail))?;
            if external_pause.is_none() {
                self.session
                    .as_ref()
                    .expect("live session exists during self-switch")
                    .begin_self_switch(generation)?;
            }
            self.topology_client()?
                .reconcile_and_suspend_input(LIVE_INPUT_LIFECYCLE_TIMEOUT)
                .map_err(KmsLiveError::Setup)?;
            let mut mailbox = PauseCollectingMailbox::new(
                self.session
                    .as_mut()
                    .expect("live session exists during render suspend"),
                &mut external_pause,
            );
            match drive_live_transition(
                &mut mailbox,
                adapter,
                commands,
                None,
                suspend_deadline,
                now,
            )? {
                LiveTransitionOutcome::Suspended {
                    generation: suspended,
                } if suspended == generation => {}
                outcome => {
                    return Err(KmsLiveError::Setup(format!(
                        "self-switch render suspend ended unexpectedly: {outcome:?}"
                    )));
                }
            }
            self.lifecycle
                .as_mut()
                .expect("live lifecycle exists after adapter start")
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation })
                .map_err(|error| KmsLiveError::Setup(error.detail))?;
            self.session
                .as_ref()
                .expect("live session exists during self-switch")
                .close_original()?;
            let mut mailbox = PauseCollectingMailbox::new(
                self.session
                    .as_mut()
                    .expect("live session exists during final pause arbitration"),
                &mut external_pause,
            );
            if let Some(event) = mailbox.poll_event()? {
                let reply =
                    classify_transition_wait_event(event, "post-suspend pause arbitration")?;
                return Err(unexpected_pump_reply(
                    "post-suspend pause arbitration",
                    reply,
                ));
            }
            if let Some(pause) = external_pause.as_ref()
                && pause.generation != generation
            {
                return Err(KmsLiveError::Setup(format!(
                    "kms-live-stale-generation: late external pause {} does not match suspended generation {generation}",
                    pause.generation
                )));
            }
            Ok(generation)
        })();
        let generation = match prepared {
            Ok(generation) => generation,
            Err(error) => {
                if let Some(pause) = external_pause.take() {
                    adapter.begin_stop();
                    self.pending_external_pause_ack = Some(pause.acknowledgement);
                    return Err(error);
                }
                if !defer_vt_switch_after_transition_failure(
                    &mut self.pending_vt_switch,
                    vt,
                    &error,
                ) {
                    // External authority loss or a terminal render frame wins
                    // the race. This chord is stale and must not submit a VT
                    // change during teardown.
                    return Err(error);
                }
                // `finish_live_operation` consumes the adapter through its
                // bounded quiesce-or-detach barrier before
                // `after_adapter_shutdown` submits this queued switch.
                return Err(KmsLiveError::Setup(format!(
                    "{error}; the VT switch is queued until render quiesces or detaches"
                )));
            }
        };
        if let Some(pause) = external_pause {
            return self.finish_external_pause_and_resume(
                adapter,
                grant,
                generation,
                pause.acknowledgement,
                now,
            );
        }
        let switch = self
            .session
            .as_ref()
            .expect("live session exists during self-switch")
            .request_self_vt_switch(vt);
        // A deferred-notifier request that wins before change_vt submission
        // makes this chord stale. The external pause coordinator owns that
        // generation; no second VT request may be deferred.
        if let Err(error) = require_accepted_self_switch(vt, switch) {
            if is_external_authority_loss(&error) {
                let mut late_pause = None;
                let mut mailbox = PauseCollectingMailbox::new(
                    self.session
                        .as_mut()
                        .expect("live session exists during late pause arbitration"),
                    &mut late_pause,
                );
                if let Some(event) = mailbox.poll_event()? {
                    let reply = classify_transition_wait_event(
                        event,
                        "post-self-switch pause arbitration",
                    )?;
                    return Err(unexpected_pump_reply(
                        "post-self-switch pause arbitration",
                        reply,
                    ));
                }
                if let Some(pause) = late_pause {
                    if pause.generation != generation {
                        self.pending_external_pause_ack = Some(pause.acknowledgement);
                        adapter.begin_stop();
                        return Err(KmsLiveError::Setup(format!(
                            "kms-live-stale-generation: late external pause {} does not match self-switch generation {generation}",
                            pause.generation
                        )));
                    }
                    return self.finish_external_pause_and_resume(
                        adapter,
                        grant,
                        generation,
                        pause.acknowledgement,
                        now,
                    );
                }
            }
            return Err(error);
        }
        self.wait_for_self_pause_confirmation(generation)?;
        self.resume_cycle = self.resume_cycle.saturating_add(1);
        self.log_paused_boundary(generation, LivePauseCause::SelfSwitch);
        self.wait_for_activate(generation, LivePauseCause::SelfSwitch)?;
        self.resume_after_activate(adapter, grant, generation, LivePauseCause::SelfSwitch, now)
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
enum ResumeAttemptFailure {
    Retry(KmsLiveError),
    Terminal(KmsLiveError),
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn resume_authority_open_is_retryable(error: &KmsLiveError) -> bool {
    matches!(
        error,
        KmsLiveError::Refused(
            KmsLiveRefusal::SessionInactiveBeforeAuthorityOpen | KmsLiveRefusal::DrmNodeOpenFailed
        )
    )
}

#[cfg(all(feature = "kms-live", not(test)))]
impl From<KmsLiveError> for ResumeAttemptFailure {
    fn from(error: KmsLiveError) -> Self {
        Self::Terminal(error)
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
impl LiveActPlatform for PreparedLiveOperation {
    type Lease = MasterDrmLease;
    type SelectedTarget = LiveSelectedTarget;
    type Protocol = crate::protocol::WaylandRuntime;
    type Adapter = super::render::LiveRenderPump;

    fn before_authority_open(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
        self.arbitrate_coordinator_event("before the DRM authority open", true)
    }

    fn open_authorised_device(&mut self, device_path: &Path) -> Result<OwnedFd, KmsLiveError> {
        self.session
            .as_ref()
            .expect("production preparation installs the session owner")
            .open(device_path)
    }

    fn duplicate_lease(&mut self) -> Result<Self::Lease, KmsLiveError> {
        self.session
            .as_ref()
            .expect("production preparation installs the session owner")
            .duplicate_lease()
    }

    fn discard_verification_fd(&mut self, verified: &mut VerifiedDrmFd) {
        drop(verified.fd.take());
    }

    fn select_target(
        &mut self,
        verified: &VerifiedDrmFd,
    ) -> Result<Self::SelectedTarget, KmsLiveError> {
        let verification_fd = verified
            .fd
            .as_ref()
            .expect("target selection precedes verification-fd disposal");
        select_live_target(
            self.output_selector
                .as_ref()
                .expect("production preparation installs the output selector"),
            verification_fd.as_fd(),
            verified,
            self.output_scale,
            None,
            None,
        )
    }

    fn start_protocol(
        &mut self,
        target: &Self::SelectedTarget,
    ) -> Result<Self::Protocol, KmsLiveError> {
        let session = self
            .session
            .as_ref()
            .expect("production preparation installs the session owner");
        let input_source = session.input_source();
        let vt_switch_events = session.event_sender();
        let protocol_failure = session.fatal.clone();
        #[cfg(feature = "bus")]
        let mut runtime = crate::protocol::WaylandRuntime::new_with_input_source_production(
            crate::DEFAULT_SOCKET,
            super::BackendKind::Kms,
            (target.bootstrap_extent.0, target.bootstrap_extent.1),
            self.protocol_wiring
                .take()
                .expect("production preparation installs protocol GPU wiring"),
            crate::protocol::WaylandRuntimePolicy {
                keybindings_enabled: true,
                explicit_sync_exposure_mode: crate::protocol::ExplicitSyncExposureMode::Production,
                decoration: self.decoration.clone(),
            },
            crate::protocol::LiveInputWiring::new(input_source, move |vt| {
                let _ = vt_switch_events.send(LiveCoordinatorEvent::VtSwitchRequested(vt));
            }),
            move || {
                let _ = protocol_failure.send_revocation(LiveRevocation::ProtocolThreadStopped);
            },
            self.bus_service.clone(),
        )
        .map_err(|error| KmsLiveError::Setup(error.to_string()))?;
        #[cfg(not(feature = "bus"))]
        let mut runtime = crate::protocol::WaylandRuntime::new_with_input_source(
            crate::DEFAULT_SOCKET,
            super::BackendKind::Kms,
            (target.bootstrap_extent.0, target.bootstrap_extent.1),
            self.protocol_wiring
                .take()
                .expect("production preparation installs protocol GPU wiring"),
            crate::protocol::WaylandRuntimePolicy {
                keybindings_enabled: true,
                explicit_sync_exposure_mode: crate::protocol::ExplicitSyncExposureMode::Production,
                decoration: self.decoration.clone(),
            },
            crate::protocol::LiveInputWiring::new(input_source, move |vt| {
                let _ = vt_switch_events.send(LiveCoordinatorEvent::VtSwitchRequested(vt));
            }),
            move || {
                let _ = protocol_failure.send_revocation(LiveRevocation::ProtocolThreadStopped);
            },
        )
        .map_err(|error| KmsLiveError::Setup(error.to_string()))?;
        runtime
            .submit_kms_topology_lifecycle(
                super::kms::KmsTopologyLifecycleEvent::Initial(target.topology.clone()),
                LIVE_TOPOLOGY_ACK_TIMEOUT,
            )
            .map_err(KmsLiveError::Setup)?;
        #[cfg(feature = "bus")]
        runtime.start_port().map_err(KmsLiveError::Setup)?;
        self.topology_client = Some(runtime.kms_topology_client());
        self.frame_clock = Some(runtime.client_frame_clock());
        self.security_reporter = Some(runtime.security_presentation_reporter());
        if self.scene_mode == LiveSceneMode::ClientContent {
            self.scene_feed = Some(
                runtime
                    .take_client_scene_feed()
                    .map_err(KmsLiveError::Setup)?,
            );
        }
        self.initial_render_commands = runtime
            .drain_kms_render_commands()
            .map_err(KmsLiveError::Setup)?;
        self.selected_output =
            self.initial_render_commands
                .iter()
                .find_map(|command| match command {
                    KmsRenderCommand::AddOutput { output, .. } => Some(output.clone()),
                    _ => None,
                });
        let output = self.selected_output.as_ref().ok_or_else(|| {
            KmsLiveError::Setup("authorised connector has no protocol-admitted mode".into())
        })?;
        tracing::info!(
            connector = output.key.connector_name,
            physical_width = output.connector_mode.width,
            physical_height = output.connector_mode.height,
            logical_width = output.logical_rect.width,
            logical_height = output.logical_rect.height,
            scale = %output.output_scale,
            scale120 = output.output_scale.get(),
            "KMS physical-to-logical output mapping admitted"
        );
        Ok(runtime)
    }

    fn before_protocol_start(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
        self.arbitrate_coordinator_event("before Wayland protocol startup", false)
    }

    fn adapter_start_decision(&mut self) -> AdapterStartDecision {
        let (decision, pause) = self
            .session
            .as_mut()
            .expect("production preparation installs the session owner")
            .adapter_start_decision();
        if let Some(pause) = pause {
            tracing::info!(
                generation = pause.generation,
                "external pause was claimed during adapter-start arbitration"
            );
            self.pending_external_pause_ack = Some(pause.acknowledgement);
        }
        if let AdapterStartDecision::EndVtSwitch(vt) = decision {
            self.pending_vt_switch = Some(vt);
        }
        decision
    }

    fn start_adapter(
        &mut self,
        lease: Self::Lease,
        _target: Self::SelectedTarget,
    ) -> Result<Self::Adapter, KmsLiveError> {
        let mut pump = self
            .pump
            .take()
            .expect("production preparation installs the live render pump");
        self.resume_mode = self
            .selected_output
            .as_ref()
            .map(|output| output.connector_mode);
        let initial_generation = self
            .initial_render_commands
            .last()
            .map(render_command_generation)
            .ok_or_else(|| KmsLiveError::Setup("initial render transition is empty".into()))?;
        self.lifecycle = Some(LiveCoordinatorLifecycle::active(
            initial_generation,
            Duration::ZERO,
        ));
        pump.start(
            lease,
            selected_output_for_adapter_start(&self.selected_output)
                .expect("protocol topology admitted the selected output"),
            std::mem::take(&mut self.initial_render_commands),
            self.topology_client
                .clone()
                .expect("protocol startup installs the topology client"),
            self.scene_feed.take(),
        )?;
        Ok(pump)
    }

    fn wait_for_revocation(
        &mut self,
        adapter: &mut Self::Adapter,
        verified: &VerifiedDrmFd,
        grant: &KmsLiveGrant,
    ) -> Result<SessionTeardown, KmsLiveError> {
        let started = Instant::now();
        let initial_generation = match self
            .lifecycle
            .as_ref()
            .expect("live lifecycle exists after adapter start")
            .state
        {
            LiveCoordinatorLifecycleState::Active { generation } => generation,
            state => unreachable!("newly started live lifecycle is not active: {state:?}"),
        };
        let mut resumed_ready = None;
        loop {
            let end = if let Some(resumed) = resumed_ready {
                let frame_clock = self.frame_clock.clone().ok_or_else(|| {
                    KmsLiveError::Setup("client frame clock is unavailable".into())
                })?;
                let security_reporter = self.security_reporter.clone().ok_or_else(|| {
                    KmsLiveError::Setup("security presentation reporter is unavailable".into())
                })?;
                let topology_client = self.topology_client()?.clone();
                match supervise_resumed_live_render(
                    self.session
                        .as_mut()
                        .expect("production preparation installs the session owner"),
                    adapter,
                    resumed,
                    self.scene_mode,
                    || started.elapsed(),
                    move |timeout| {
                        topology_client
                            .flush_events(timeout)
                            .map_err(KmsLiveError::Setup)
                    },
                    move || frame_clock.pulse().map_err(KmsLiveError::Setup),
                    move |presentation_epoch, generation, output| {
                        security_reporter
                            .kms_presented(presentation_epoch, generation, output)
                            .map_err(KmsLiveError::Setup)
                    },
                )? {
                    LiveSupervisionEnd::Revocation(revocation) => {
                        adapter.begin_stop();
                        ActiveLiveOperationEnd::Revocation {
                            revocation,
                            teardown: session_teardown_after(Some(revocation)),
                        }
                    }
                    LiveSupervisionEnd::Signal(signal) => {
                        adapter.begin_stop();
                        return Err(KmsLiveError::Signal(signal));
                    }
                    LiveSupervisionEnd::VtSwitchRequested {
                        vt,
                        outstanding_command,
                    } => ActiveLiveOperationEnd::VtSwitchRequested {
                        vt,
                        outstanding_command,
                    },
                    LiveSupervisionEnd::PauseRequested {
                        generation,
                        acknowledgement,
                        outstanding_command,
                    } => ActiveLiveOperationEnd::PauseRequested {
                        generation,
                        acknowledgement,
                        outstanding_command,
                    },
                }
            } else {
                let session = self
                    .session
                    .as_mut()
                    .expect("production preparation installs the session owner");
                let active_fd_baseline = &mut self.active_fd_baseline;
                let frame_clock = self.frame_clock.clone().ok_or_else(|| {
                    KmsLiveError::Setup("client frame clock is unavailable".into())
                })?;
                let security_reporter = self.security_reporter.clone().ok_or_else(|| {
                    KmsLiveError::Setup("security presentation reporter is unavailable".into())
                })?;
                supervise_active_live_operation_after_output_ready(
                    session,
                    adapter,
                    || started.elapsed(),
                    |_| {
                        // Initial Active regime: target creation is complete at
                        // OutputReady, so stable target fds belong in the baseline.
                        Self::log_output_ready_active_boundary(
                            active_fd_baseline,
                            initial_generation,
                        );
                    },
                    move || frame_clock.pulse().map_err(KmsLiveError::Setup),
                    move |presentation_epoch, generation, output| {
                        security_reporter
                            .kms_presented(presentation_epoch, generation, output)
                            .map_err(KmsLiveError::Setup)
                    },
                )?
            };
            match end {
                ActiveLiveOperationEnd::VtSwitchRequested {
                    vt,
                    outstanding_command,
                } => {
                    let mut now = || started.elapsed();
                    let resumed = self.self_switch_and_resume(
                        adapter,
                        grant,
                        vt,
                        outstanding_command,
                        &mut now,
                    );
                    resumed_ready = Some(match resumed {
                        Ok(resumed) => resumed,
                        Err(KmsLiveError::ExternalPauseRequested {
                            generation,
                            acknowledgement,
                        }) => {
                            adapter.begin_stop();
                            self.pending_external_pause_ack = Some(acknowledgement);
                            return Err(KmsLiveError::Setup(format!(
                                "external pause generation {generation} interrupted the self-switch transition; the renderer will detach before the protocol acknowledgement"
                            )));
                        }
                        Err(error) => return Err(error),
                    });
                    continue;
                }
                ActiveLiveOperationEnd::PauseRequested {
                    generation,
                    acknowledgement,
                    outstanding_command,
                } => {
                    let mut now = || started.elapsed();
                    let resumed = self.external_pause_and_resume(
                        adapter,
                        grant,
                        generation,
                        acknowledgement,
                        outstanding_command,
                        &mut now,
                    );
                    resumed_ready = Some(match resumed {
                        Ok(resumed) => resumed,
                        Err(KmsLiveError::ExternalPauseRequested {
                            generation,
                            acknowledgement,
                        }) => {
                            adapter.begin_stop();
                            self.pending_external_pause_ack = Some(acknowledgement);
                            return Err(KmsLiveError::Setup(format!(
                                "external pause generation {generation} interrupted a pause transition; the renderer will detach before the protocol acknowledgement"
                            )));
                        }
                        Err(error) => return Err(error),
                    });
                    continue;
                }
                ActiveLiveOperationEnd::Revocation {
                    revocation,
                    teardown,
                } => {
                    match revocation {
                        LiveRevocation::SessionPause | LiveRevocation::TargetHotplug => {
                            log_live_authority_revoked(revocation, &verified.stable_device_path);
                        }
                        LiveRevocation::SessionUnresponsive => tracing::error!(
                            device = %verified.stable_device_path.display(),
                            "the compositor session thread stopped answering an input-device operation"
                        ),
                        LiveRevocation::SessionThreadStopped => tracing::error!(
                            device = %verified.stable_device_path.display(),
                            "the compositor session thread stopped unexpectedly"
                        ),
                        LiveRevocation::ProtocolThreadStopped => tracing::error!(
                            device = %verified.stable_device_path.display(),
                            "the compositor Wayland protocol thread stopped unexpectedly"
                        ),
                    }
                    return if matches!(revocation, LiveRevocation::ProtocolThreadStopped) {
                        Err(KmsLiveError::Setup(
                            "the Wayland protocol thread stopped before the live operation ended"
                                .into(),
                        ))
                    } else {
                        Ok(teardown)
                    };
                }
            }
        }
    }

    fn shutdown_adapter(&mut self, adapter: Self::Adapter) -> Result<(), KmsLiveError> {
        adapter.shutdown()
    }

    fn after_adapter_shutdown(&mut self) -> Result<(), KmsLiveError> {
        let external_pause_acknowledgement = self.pending_external_pause_ack.take();
        // A request queued during protocol startup never moved this handle into
        // `adapter`; its unstarted pump still passes through the same bounded
        // quiescence barrier. A failed active transition sets the same pending
        // request after `shutdown_adapter` consumed its adapter. In either case
        // the backend has already revoked device authority; the remaining order
        // is local render quiescence, input/DRM close, then protocol ack.
        let pending_pump_shutdown =
            if self.pending_vt_switch.is_some() || external_pause_acknowledgement.is_some() {
                self.pump
                    .take()
                    .map(super::render::LiveRenderPump::shutdown)
                    .unwrap_or(Ok(()))
            } else {
                Ok(())
            };
        let external_pause_cleanup = if external_pause_acknowledgement.is_some() {
            self.cleanup_revoked_external_pause_devices()
        } else {
            Ok(())
        };
        let external_pause_acknowledged = external_pause_acknowledgement
            .map(ExternalPauseAcknowledgement::acknowledge)
            .unwrap_or(true);
        let pending_pump_shutdown =
            combine_live_results(pending_pump_shutdown, external_pause_cleanup);
        let Some(vt) = self.pending_vt_switch.take() else {
            return combine_live_results(
                pending_pump_shutdown,
                external_pause_acknowledged.then_some(()).ok_or_else(|| {
                    KmsLiveError::Setup(
                        "external pause acknowledgement waiter vanished after render shutdown"
                            .into(),
                    )
                }),
            );
        };
        let outcome = self
            .session
            .as_ref()
            .expect("the live session exists until the teardown funnel closes it")
            .request_vt_switch(vt);
        if outcome != VtSwitchAsk::Accepted {
            tracing::warn!(vt, ?outcome, "live VT-switch request was not accepted");
        }
        combine_live_results(
            pending_pump_shutdown,
            external_pause_acknowledged.then_some(()).ok_or_else(|| {
                KmsLiveError::Setup(
                    "external pause acknowledgement waiter vanished after render shutdown".into(),
                )
            }),
        )
    }

    fn stop_protocol(&mut self, protocol: Self::Protocol) {
        drop(protocol);
    }

    fn close_session(&mut self, teardown: SessionTeardown) -> Result<(), KmsLiveError> {
        // `close`, not `shutdown` or `abandon` directly: it re-decides the
        // teardown against whatever the revocations channel still holds, and its
        // graceful path is bounded so that a thread which wedges after the
        // decision was taken is detached rather than waited on.
        let shutdown = match self.session.take() {
            Some(session) => session.close(teardown),
            None => Ok(()),
        };
        drop(self.signals.take());
        drop(self.pump.take());
        drop(self.protocol_wiring.take());
        drop(self.output_selector.take());
        drop(self.topology_client.take());
        shutdown
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct ResumeSynchronousBudget<'a> {
    deadline: Duration,
    now: &'a mut dyn FnMut() -> Duration,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl ResumeSynchronousBudget<'_> {
    fn boundary(&mut self, stage: &'static str) -> Result<(), KmsLiveError> {
        run_resume_synchronous_stage(self.deadline, (self.now)(), stage, || Ok(()))
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
#[allow(irrefutable_let_patterns)]
fn select_live_target(
    selector: &super::render::PreparedLiveOutputSelector,
    lease: BorrowedFd<'_>,
    verified: &VerifiedDrmFd,
    output_scale: OutputScale120,
    required_mode: Option<super::kms::ConnectorMode>,
    mut resume_budget: Option<ResumeSynchronousBudget<'_>>,
) -> Result<LiveSelectedTarget, KmsLiveError> {
    // Like the post-open validation above, these synchronous driver probes have
    // no interruptible timeout. Budget enforcement is honest boundary refusal;
    // a probe wedged after entry remains the dead-man rescue's responsibility.
    if let Some(budget) = resume_budget.as_mut() {
        budget.boundary("live target DRM scan")?;
    }
    let scan = scan_borrowed_card(
        verified.device_id,
        &verified.device_path,
        lease,
        Path::new("/sys/class/drm"),
        ConnectorProbe::Forced,
        |_| panic!("the sealed live adapter scans only its libseat lease"),
        borrowed_master_state,
    )
    .map_err(|error| KmsLiveError::Setup(format!("live connector scan failed: {error}")))?;
    let mut connector = scan
        .connectors()
        .find(|connector| {
            connector.name == verified.connector_name
                && connector.connector_id == verified.connector_id
                && connector.status == ConnectorStatus::Connected
        })
        .and_then(|connector| connector.description())
        .ok_or_else(|| KmsLiveError::Setup("authorised connector was revoked".into()))?;
    if let Some(required) = required_mode {
        retain_exact_prior_mode(&mut connector.modes, required)?;
    }
    if let Some(budget) = resume_budget.as_mut() {
        budget.boundary("atomic output admission")?;
    }
    let selection = super::atomic_present::admit_atomic_output_from_fd(
        lease,
        verified.device_id,
        verified.connector_id,
        &verified.connector_name,
        selector.0.clone(),
    )
    .map_err(|error| KmsLiveError::Setup(format!("kms-live-atomic-admission-failed: {error}")))?;
    if !connector.modes.contains(&selection.mode) {
        return Err(KmsLiveError::Setup(format!(
            "kms-live-atomic-mode-mismatch: admitted mode {}x{}@{}mHz is not the required connector timing",
            selection.mode.width, selection.mode.height, selection.mode.refresh_millihz
        )));
    }
    let selections = connector
        .modes
        .iter()
        .copied()
        .map(|mode| super::kms::PreselectedAtomicOutput {
            key: connector.key.clone(),
            connector_mode: mode,
            selection: (mode == selection.mode)
                .then_some(selection)
                .ok_or_else(|| "atomic admission selected a different exact mode".to_string()),
        })
        .collect();
    let topology = super::kms::KmsTopologySnapshot {
        connectors: vec![connector],
        selections,
        output_scale,
    };
    let bootstrap_extent = admitted_bootstrap_extent(&topology)?;
    Ok(LiveSelectedTarget {
        topology,
        bootstrap_extent,
    })
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn retain_exact_prior_mode(
    modes: &mut Vec<super::kms::ConnectorMode>,
    required: super::kms::ConnectorMode,
) -> Result<(), KmsLiveError> {
    modes.retain(|mode| *mode == required);
    if modes.is_empty() {
        Err(KmsLiveError::Setup(format!(
            "kms-live-prior-mode-missing: exact prior mode {}x{}@{}mHz is unavailable",
            required.width, required.height, required.refresh_millihz
        )))
    } else {
        Ok(())
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn admitted_bootstrap_extent(
    topology: &super::kms::KmsTopologySnapshot,
) -> Result<(u32, u32), KmsLiveError> {
    let commands = super::kms::KmsTopology::default()
        .reduce_lifecycle(super::kms::KmsTopologyLifecycleEvent::Initial(
            topology.clone(),
        ))
        .map_err(|error| KmsLiveError::Setup(error.to_string()))?;
    commands
        .into_iter()
        .find_map(|command| match command {
            super::kms::KmsRenderCommand::AddOutput { output, .. } => Some((
                u32::try_from(output.logical_rect.width).ok()?,
                u32::try_from(output.logical_rect.height).ok()?,
            )),
            _ => None,
        })
        .ok_or_else(|| KmsLiveError::Setup("authorised connector has no admitted mode".into()))
}

fn parse_request(argv: &[OsString]) -> Result<KmsLiveRequest, KmsLiveRefusal> {
    if argv.first().and_then(|argument| argument.to_str()) != Some(KMS_LIVE_SUBCOMMAND) {
        return Err(KmsLiveRefusal::SubcommandNotFirst);
    }
    let mut device = None;
    let mut connector = None;
    let mut output_scale = None;
    let mut presentation_backend = None;
    let mut scene_mode = LiveSceneMode::ClientContent;
    let mut ssd = None;
    let mut chrome = None;
    let mut confirm = false;
    let mut index = 1;
    while index < argv.len() {
        let Some(argument) = argv[index].to_str() else {
            return Err(KmsLiveRefusal::UnknownArgument);
        };
        match argument {
            "--device" => {
                if device.is_some() {
                    return Err(KmsLiveRefusal::DuplicateDevice);
                }
                device = Some(required_value(
                    argv,
                    &mut index,
                    KmsLiveRefusal::MissingDevice,
                )?);
            }
            "--connector" => {
                if connector.is_some() {
                    return Err(KmsLiveRefusal::DuplicateConnector);
                }
                connector = Some(required_value(
                    argv,
                    &mut index,
                    KmsLiveRefusal::MissingConnector,
                )?);
            }
            "--scale" => {
                if output_scale.is_some() {
                    return Err(KmsLiveRefusal::DuplicateScale);
                }
                let value = required_value(argv, &mut index, KmsLiveRefusal::MissingScale)?;
                output_scale = Some(parse_output_scale(&value)?);
            }
            "--presentation" => {
                if presentation_backend.is_some() {
                    return Err(KmsLiveRefusal::DuplicatePresentation);
                }
                let value = required_value(argv, &mut index, KmsLiveRefusal::MissingPresentation)?;
                presentation_backend = Some(match value.as_str() {
                    "atomic" => PresentationBackend::Atomic,
                    "direct-display" => return Err(KmsLiveRefusal::DirectDisplayRetired),
                    _ => return Err(KmsLiveRefusal::InvalidPresentation),
                });
            }
            "--first-light" => {
                if scene_mode == LiveSceneMode::FirstLight {
                    return Err(KmsLiveRefusal::DuplicateFirstLight);
                }
                scene_mode = LiveSceneMode::FirstLight;
            }
            "--ssd" => {
                if let Some(enabled) = ssd {
                    return Err(if enabled {
                        KmsLiveRefusal::DuplicateSsd
                    } else {
                        KmsLiveRefusal::SsdNoSsdConflict
                    });
                }
                ssd = Some(true);
            }
            "--no-ssd" => {
                if let Some(enabled) = ssd {
                    return Err(if enabled {
                        KmsLiveRefusal::SsdNoSsdConflict
                    } else {
                        KmsLiveRefusal::DuplicateNoSsd
                    });
                }
                if chrome.is_some() {
                    return Err(KmsLiveRefusal::NoSsdChromeConflict);
                }
                ssd = Some(false);
            }
            "--chrome" => {
                if chrome.is_some() {
                    return Err(KmsLiveRefusal::DuplicateChrome);
                }
                let value = required_value(argv, &mut index, KmsLiveRefusal::MissingChrome)?;
                chrome = Some(ChromeStyle::from_name(&value).ok_or(KmsLiveRefusal::InvalidChrome)?);
                if ssd == Some(false) {
                    return Err(KmsLiveRefusal::NoSsdChromeConflict);
                }
            }
            "--kms-confirm" => {
                if confirm {
                    return Err(KmsLiveRefusal::DuplicateKmsConfirm);
                }
                confirm = true;
            }
            _ => return Err(KmsLiveRefusal::UnknownArgument),
        }
        index += 1;
    }
    let device = PathBuf::from(device.ok_or(KmsLiveRefusal::MissingDevice)?);
    if !device.is_absolute() {
        return Err(KmsLiveRefusal::InvalidDevice);
    }
    let connector = connector.ok_or(KmsLiveRefusal::MissingConnector)?;
    if connector.is_empty()
        || !connector
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(KmsLiveRefusal::InvalidConnector);
    }
    if scene_mode == LiveSceneMode::FirstLight && (ssd == Some(true) || chrome.is_some()) {
        return Err(KmsLiveRefusal::DecorationFirstLightConflict);
    }
    let decorations_enabled =
        scene_mode != LiveSceneMode::FirstLight && (ssd.unwrap_or(true) || chrome.is_some());
    Ok(KmsLiveRequest {
        device,
        connector,
        presentation_backend: presentation_backend.unwrap_or_default(),
        scene_mode,
        output_scale: output_scale.unwrap_or(OutputScale120::ONE),
        decoration: DecorationStartup::resolve(
            decorations_enabled,
            chrome.unwrap_or(ChromeStyle::Mac),
        ),
        confirm,
    })
}

fn parse_output_scale(value: &str) -> Result<OutputScale120, KmsLiveRefusal> {
    if value.bytes().any(|byte| byte.is_ascii_alphabetic()) {
        return Err(KmsLiveRefusal::InvalidScale);
    }
    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    if negative {
        return Err(KmsLiveRefusal::NonPositiveScale);
    }
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) if !fraction.contains('.') => (whole, fraction),
        Some(_) => return Err(KmsLiveRefusal::InvalidScale),
        None => (unsigned, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(KmsLiveRefusal::InvalidScale);
    }

    let whole = whole
        .parse::<u128>()
        .map_err(|_| KmsLiveRefusal::InvalidScale)?;
    let fraction = fraction.trim_end_matches('0');
    let fractional_120ths = if fraction.is_empty() {
        0
    } else {
        let numerator = fraction
            .parse::<u128>()
            .map_err(|_| KmsLiveRefusal::InvalidScale)?
            .checked_mul(120)
            .ok_or(KmsLiveRefusal::InvalidScale)?;
        let denominator = 10_u128
            .checked_pow(u32::try_from(fraction.len()).map_err(|_| KmsLiveRefusal::InvalidScale)?)
            .ok_or(KmsLiveRefusal::Non120thScale)?;
        if !numerator.is_multiple_of(denominator) {
            return Err(KmsLiveRefusal::Non120thScale);
        }
        numerator / denominator
    };
    let scale120 = whole
        .checked_mul(120)
        .and_then(|whole| whole.checked_add(fractional_120ths))
        .and_then(|scale| u32::try_from(scale).ok())
        .ok_or(KmsLiveRefusal::InvalidScale)?;
    OutputScale120::new(scale120).ok_or(KmsLiveRefusal::NonPositiveScale)
}

fn required_value(
    argv: &[OsString],
    index: &mut usize,
    missing: KmsLiveRefusal,
) -> Result<String, KmsLiveRefusal> {
    *index += 1;
    argv.get(*index)
        .and_then(|argument| argument.to_str())
        .filter(|value| !value.starts_with("--"))
        .map(str::to_owned)
        .ok_or(missing)
}

fn confirmation_code(nonce: &[u8; CONFIRMATION_NONCE_BYTES]) -> String {
    let mut code = String::with_capacity(CONFIRMATION_NONCE_BYTES * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in nonce {
        code.push(HEX[usize::from(byte >> 4)] as char);
        code.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    code
}

#[cfg(any(feature = "kms-live", test))]
fn is_primary_card_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.strip_prefix("card").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

#[cfg(any(feature = "kms-live", test))]
fn connector_name_from_sysfs(card_name: &str, entry_name: &OsStr) -> Option<String> {
    entry_name
        .to_str()?
        .strip_prefix(&format!("{card_name}-"))
        .filter(|connector| !connector.is_empty())
        .map(str::to_owned)
}

#[cfg(any(feature = "kms-live", test))]
struct DeviceNodeObservation {
    canonical_path: PathBuf,
    node_is_character_device: bool,
    node_rdev: u64,
    udev_sysname: Option<OsString>,
    udev_rdev: Option<u64>,
    stable_device_path: Option<PathBuf>,
}

#[cfg(any(feature = "kms-live", test))]
fn device_identity_from_observation(
    request: &KmsLiveRequest,
    observation: DeviceNodeObservation,
    sysfs_entry_names: impl IntoIterator<Item = OsString>,
) -> Option<DeviceIdentity> {
    let card_name = observation.canonical_path.file_name()?.to_str()?.to_owned();
    let connectors = sysfs_entry_names
        .into_iter()
        .filter_map(|entry_name| connector_name_from_sysfs(&card_name, &entry_name))
        .collect();
    Some(DeviceIdentity {
        observation_available: true,
        observed_for: request.device.clone(),
        canonical_path: Some(observation.canonical_path),
        node_is_character_device: observation.node_is_character_device,
        node_is_primary_drm: is_primary_card_name(OsStr::new(&card_name))
            && observation
                .udev_sysname
                .as_deref()
                .is_some_and(|name| name == OsStr::new(&card_name)),
        node_rdev: observation.node_rdev,
        udev_rdev: observation.udev_rdev,
        stable_device_path: observation.stable_device_path,
        connectors,
    })
}

#[cfg(any(feature = "kms-live", test))]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LinuxVtStat {
    active: u16,
    signal: u16,
    state: u16,
}

#[cfg(any(feature = "kms-live", test))]
const VT_GETSTATE: libc::c_ulong = 0x5603;

#[cfg(all(feature = "kms-live", not(test)))]
struct LibcTtyKernelCalls;

#[cfg(all(feature = "kms-live", not(test)))]
impl TtyKernelCalls for LibcTtyKernelCalls {
    fn tcflush(&self, fd: libc::c_int, selector: libc::c_int) -> libc::c_int {
        // SAFETY: the caller supplies a live terminal fd and a termios selector.
        unsafe { libc::tcflush(fd, selector) }
    }

    fn tcgetpgrp(&self, fd: libc::c_int) -> libc::pid_t {
        // SAFETY: this reads process-group state from the live terminal fd.
        unsafe { libc::tcgetpgrp(fd) }
    }

    fn getpgrp(&self) -> libc::pid_t {
        // SAFETY: getpgrp has no pointer or lifetime requirements.
        unsafe { libc::getpgrp() }
    }

    fn tiocgdev(&self, fd: libc::c_int, request: libc::c_ulong, output: &mut u32) -> libc::c_int {
        // SAFETY: output has the writable representation expected by TIOCGDEV.
        unsafe { libc::ioctl(fd, request, output) }
    }

    fn vt_getstate(
        &self,
        fd: libc::c_int,
        request: libc::c_ulong,
        output: &mut LinuxVtStat,
    ) -> libc::c_int {
        // SAFETY: output has the writable representation expected by VT_GETSTATE.
        unsafe { libc::ioctl(fd, request, output) }
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
struct LinuxPlatform {
    tty_kernel: Rc<dyn TtyKernelCalls>,
}

#[cfg(all(feature = "kms-live", not(test)))]
impl GrantPlatform for LinuxPlatform {
    fn observe_vt(&self, tty: BorrowedFd<'_>) -> VtState {
        let Some(stat) = fstat(tty) else {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "fstat failed on the controlling-terminal alias"
            );
            return VtState::default();
        };
        observe_vt_after_fstat(
            self.tty_kernel.as_ref(),
            tty,
            stat.st_mode & libc::S_IFMT == libc::S_IFCHR,
            stat.st_rdev,
        )
    }

    fn observe_device(&self, request: &KmsLiveRequest) -> DeviceIdentity {
        observe_device_identity(request)
    }

    fn legacy_tiocsti_enabled(&self) -> Result<bool, KmsLiveRefusal> {
        let raw = fs::read_to_string("/proc/sys/dev/tty/legacy_tiocsti")
            .map_err(|_| KmsLiveRefusal::TtyLegacyInjectionStateUnavailable)?;
        raw.trim()
            .parse::<i64>()
            .map(|value| value != 0)
            .map_err(|_| KmsLiveRefusal::TtyLegacyInjectionStateUnavailable)
    }

    fn fill_confirmation_nonce(&self, nonce: &mut [u8]) -> Result<(), KmsLiveRefusal> {
        getrandom::fill(nonce).map_err(|_| KmsLiveRefusal::ConfirmationNonceUnavailable)
    }

    fn hold_device_incarnation(
        &self,
        device: &DeviceIdentity,
    ) -> Result<DeviceIncarnationWitness, KmsLiveRefusal> {
        hold_sysfs_device_incarnation(device)
    }

    fn validate_device_incarnation(
        &self,
        witness: &DeviceIncarnationWitness,
        opened: &OpenDrmIdentity,
    ) -> Result<(), KmsLiveRefusal> {
        validate_sysfs_device_incarnation(witness, opened)
    }

    fn observe_open_drm(&self, fd: BorrowedFd<'_>) -> Result<OpenDrmIdentity, KmsLiveRefusal> {
        let stat = fstat(fd).ok_or(KmsLiveRefusal::DrmNodeObservationUnavailable)?;
        open_drm_identity_from_rdev(stat.st_rdev)
            .ok_or(KmsLiveRefusal::DeviceStableIdentityUnavailable)
    }

    fn scan_connector(
        &self,
        fd: BorrowedFd<'_>,
        opened: &OpenDrmIdentity,
        connector_name: &str,
    ) -> Result<Option<ConnectorBinding>, KmsLiveRefusal> {
        scan_connected_connector(fd, opened, connector_name)
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn hold_sysfs_device_incarnation(
    device: &DeviceIdentity,
) -> Result<DeviceIncarnationWitness, KmsLiveRefusal> {
    let identity = open_drm_identity_from_rdev(device.node_rdev)
        .ok_or(KmsLiveRefusal::DeviceStableIdentityUnavailable)?;
    if device.stable_device_path.as_deref() != Some(identity.stable_device_path.as_path()) {
        return Err(KmsLiveRefusal::DeviceStableIdentityChanged);
    }
    let card_inode = fs::metadata(&identity.sysfs_card_path)
        .map_err(|_| KmsLiveRefusal::DeviceIncarnationOpenFailed)?
        .ino();
    let dev_attribute = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(identity.sysfs_card_path.join("dev"))
        .map(Into::into)
        .map_err(|_| KmsLiveRefusal::DeviceIncarnationOpenFailed)?;
    let witness = DeviceIncarnationWitness {
        dev_attribute,
        card_inode,
        expected_rdev: device.node_rdev,
    };
    let observed = read_sysfs_dev_attribute(witness.dev_attribute.as_fd())
        .map_err(|_| KmsLiveRefusal::DeviceIncarnationReadFailed)?;
    if observed != witness.expected_rdev {
        return Err(KmsLiveRefusal::DeviceIncarnationChanged);
    }
    Ok(witness)
}

#[cfg(all(feature = "kms-live", not(test)))]
fn validate_sysfs_device_incarnation(
    witness: &DeviceIncarnationWitness,
    opened: &OpenDrmIdentity,
) -> Result<(), KmsLiveRefusal> {
    let observed = read_sysfs_dev_attribute(witness.dev_attribute.as_fd()).map_err(|error| {
        if error.raw_os_error() == Some(libc::ENODEV) {
            KmsLiveRefusal::DeviceIncarnationGone
        } else {
            KmsLiveRefusal::DeviceIncarnationReadFailed
        }
    })?;
    if observed != witness.expected_rdev || opened.rdev != witness.expected_rdev {
        return Err(KmsLiveRefusal::DeviceIncarnationChanged);
    }
    let current_inode = fs::metadata(&opened.sysfs_card_path)
        .map_err(|_| KmsLiveRefusal::DeviceIncarnationChanged)?
        .ino();
    if current_inode != witness.card_inode {
        return Err(KmsLiveRefusal::DeviceIncarnationChanged);
    }
    Ok(())
}

#[cfg(all(feature = "kms-live", not(test)))]
fn read_sysfs_dev_attribute(fd: BorrowedFd<'_>) -> Result<u64, std::io::Error> {
    let mut buffer = [0_u8; 64];
    // SAFETY: `buffer` is writable for its full length, the borrowed fd remains
    // live for the call, and offset zero re-reads the held kernfs attribute
    // without mutating shared file position.
    let count = unsafe { libc::pread(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len(), 0) };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let raw = std::str::from_utf8(&buffer[..count as usize])
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let (major, minor) = raw.trim().split_once(':').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "sysfs DRM dev attribute has no major:minor separator",
        )
    })?;
    let major = major
        .parse::<u32>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(libc::makedev(major, minor))
}

#[cfg(any(feature = "kms-live", test))]
fn observe_vt_after_fstat(
    kernel: &dyn TtyKernelCalls,
    tty: BorrowedFd<'_>,
    tty_is_character_device: bool,
    tty_alias_rdev: u64,
) -> VtState {
    let mut observed = VtState {
        observation_available: true,
        tty_is_character_device,
        tty_alias_rdev,
        ..VtState::default()
    };
    // The alias identity is checked before TIOCGDEV so a bind-mounted ttyN
    // cannot be mistaken for /dev/tty.
    if !observed.tty_is_character_device
        || observed.tty_alias_rdev != libc::makedev(TTYAUX_MAJOR, TTY_ALIAS_MINOR)
    {
        return observed;
    }
    let fd = tty.as_raw_fd();
    let foreground = kernel.tcgetpgrp(fd);
    let caller = kernel.getpgrp();
    observed.foreground_process_group = foreground >= 0 && foreground == caller;
    if !observed.foreground_process_group {
        // Observed and stated, but NOT an early return: the ioctls below are
        // harmless reads, and skipping them left `active_vt` unset — which
        // `validate_vt` checks before the foreground flag, so a non-foreground
        // caller was refused as "VT observation unavailable" instead of by the
        // refusal that names it. The first real-hardware refusal was
        // misdiagnosed for exactly that reason.
        tracing::warn!(
            foreground,
            caller,
            "the compositor is not the controlling terminal's foreground process group"
        );
    }
    let mut tty_rdev = 0_u32;
    let mut raw = LinuxVtStat::default();
    // errno is captured immediately after each call: the second ioctl
    // overwrites the first's, and a refusal that cannot say which kernel call
    // failed, with what error, is undiagnosable on exactly the hardware it
    // exists to be diagnosed on.
    let device_result = kernel.tiocgdev(fd, libc::TIOCGDEV, &mut tty_rdev);
    let device_error = (device_result != 0).then(std::io::Error::last_os_error);
    let state_result = kernel.vt_getstate(fd, VT_GETSTATE, &mut raw);
    let state_error = (state_result != 0).then(std::io::Error::last_os_error);
    if device_result == 0 {
        observed.tty_major = libc::major(u64::from(tty_rdev));
        observed.tty_minor = libc::minor(u64::from(tty_rdev));
    } else {
        observed.observation_available = false;
    }
    observed.active_vt = (state_result == 0).then_some(raw.active);
    if let Some(error) = device_error {
        tracing::warn!(%error, "TIOCGDEV failed on the controlling terminal");
    }
    if let Some(error) = state_error {
        tracing::warn!(%error, "VT_GETSTATE failed on the controlling terminal");
    }
    if device_result == 0 && state_result == 0 {
        tracing::debug!(
            tty_major = observed.tty_major,
            tty_minor = observed.tty_minor,
            active_vt = raw.active,
            "VT observation succeeded"
        );
    }
    observed
}

#[cfg(all(feature = "kms-live", not(test)))]
fn open_drm_identity_from_rdev(rdev: u64) -> Option<OpenDrmIdentity> {
    let sysfs_card_path = fs::canonicalize(format!(
        "/sys/dev/char/{}:{}",
        libc::major(rdev),
        libc::minor(rdev)
    ))
    .ok()?;
    open_drm_identity_from_sysfs(rdev, sysfs_card_path)
}

#[cfg(any(feature = "kms-live", test))]
fn open_drm_identity_from_sysfs(rdev: u64, sysfs_card_path: PathBuf) -> Option<OpenDrmIdentity> {
    let card_name = sysfs_card_path.file_name()?.to_str()?;
    if !is_primary_card_name(OsStr::new(card_name)) {
        return None;
    }
    let stable_device_path = sysfs_card_path.parent()?.parent()?.to_path_buf();
    Some(OpenDrmIdentity {
        rdev,
        stable_device_path,
        sysfs_card_path,
    })
}

#[cfg(all(feature = "kms-live", not(test)))]
fn scan_connected_connector(
    fd: BorrowedFd<'_>,
    opened: &OpenDrmIdentity,
    connector_name: &str,
) -> Result<Option<ConnectorBinding>, KmsLiveRefusal> {
    let card_name = opened
        .sysfs_card_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(KmsLiveRefusal::ConnectorBoundaryScanFailed)?;
    let card_path = Path::new("/dev/dri").join(card_name);
    let scan = scan_borrowed_card(
        opened.rdev,
        &card_path,
        fd,
        Path::new("/sys/class/drm"),
        ConnectorProbe::Cached,
        |_| panic!("borrowed DRM scanning must never open the card path"),
        borrowed_master_state,
    )
    .map_err(|_| KmsLiveRefusal::ConnectorBoundaryScanFailed)?;
    Ok(scan
        .connectors()
        .find(|connector| {
            connector.name == connector_name && connector.status == ConnectorStatus::Connected
        })
        .map(|connector| ConnectorBinding {
            connector_id: connector.connector_id,
        }))
}

#[cfg(all(feature = "kms-live", not(test)))]
fn fstat(fd: BorrowedFd<'_>) -> Option<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and is read only after a
    // successful fstat initialises it.
    (unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } == 0)
        .then(|| unsafe { stat.assume_init() })
}

#[cfg(all(feature = "kms-live", not(test)))]
fn open_controlling_tty() -> Result<OwnedFd, KmsLiveRefusal> {
    let spec = tty_open_spec();
    let opened = OpenOptions::new()
        .read(spec.read)
        .write(spec.write)
        .custom_flags(spec.custom_flags)
        .open("/dev/tty")
        .ok();
    require_open_tty(opened)
}

#[cfg(any(feature = "kms-live", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TtyOpenSpec {
    read: bool,
    write: bool,
    custom_flags: i32,
}

#[cfg(any(feature = "kms-live", test))]
const fn tty_open_spec() -> TtyOpenSpec {
    TtyOpenSpec {
        read: true,
        write: true,
        custom_flags: libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_CLOEXEC,
    }
}

#[cfg(any(feature = "kms-live", test))]
fn require_open_tty<T>(opened: Option<T>) -> Result<OwnedFd, KmsLiveRefusal>
where
    T: Into<OwnedFd>,
{
    opened.map(Into::into).ok_or(KmsLiveRefusal::TtyOpenFailed)
}

#[cfg(any(feature = "kms-live", test))]
struct TtyConfirmationSource {
    tty_kernel: Rc<dyn TtyKernelCalls>,
}

#[cfg(any(feature = "kms-live", test))]
impl ConfirmationIo for TtyConfirmationSource {
    fn flush_input(&mut self, tty: BorrowedFd<'_>) -> Result<(), KmsLiveRefusal> {
        // SAFETY: tcflush changes only the input queue of this live borrowed
        // terminal. Refusal on failure is required because otherwise input
        // queued before the prompt could satisfy the interlock unattended.
        require_input_flush(self.tty_kernel.tcflush(tty.as_raw_fd(), libc::TCIFLUSH))
    }

    fn display_prompt(
        &mut self,
        tty: BorrowedFd<'_>,
        intent: &str,
        expected_code: &str,
    ) -> Result<(), KmsLiveRefusal> {
        let duplicate = tty
            .try_clone_to_owned()
            .map_err(|_| KmsLiveRefusal::ConfirmationReadFailed)?;
        let mut file = std::fs::File::from(duplicate);
        writeln!(
            file,
            "{intent}\nType this code to continue: {expected_code}"
        )
        .and_then(|()| file.flush())
        .map_err(|_| KmsLiveRefusal::ConfirmationReadFailed)
    }

    fn read_line(&mut self, tty: BorrowedFd<'_>) -> Result<String, KmsLiveRefusal> {
        let duplicate = tty
            .try_clone_to_owned()
            .map_err(|_| KmsLiveRefusal::ConfirmationReadFailed)?;
        let mut line = String::new();
        BufReader::new(std::fs::File::from(duplicate))
            .read_line(&mut line)
            .map_err(|_| KmsLiveRefusal::ConfirmationReadFailed)?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
}

#[cfg(all(feature = "kms-live", not(test)))]
fn observe_device_identity(request: &KmsLiveRequest) -> DeviceIdentity {
    observe_device_identity_inner(request)
        .unwrap_or_else(|| DeviceIdentity::unavailable_for(request.device.clone()))
}

#[cfg(all(feature = "kms-live", not(test)))]
fn observe_device_identity_inner(request: &KmsLiveRequest) -> Option<DeviceIdentity> {
    use smithay::reexports::udev;

    let canonical_path = fs::canonicalize(&request.device).ok()?;
    let metadata = fs::metadata(&canonical_path).ok()?;
    let mut enumerator = udev::Enumerator::new().ok()?;
    enumerator.match_subsystem("drm").ok()?;
    let udev_device = enumerator.scan_devices().ok()?.find(|device| {
        device
            .devnode()
            .and_then(|path| fs::canonicalize(path).ok())
            .is_some_and(|path| path == canonical_path)
    });
    let udev_rdev = udev_device.as_ref().and_then(udev::Device::devnum);
    let udev_sysname = udev_device
        .as_ref()
        .map(|device| device.sysname().to_os_string());
    let stable_device_path =
        open_drm_identity_from_rdev(metadata.rdev()).map(|identity| identity.stable_device_path);
    let sysfs_entry_names = fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    device_identity_from_observation(
        request,
        DeviceNodeObservation {
            canonical_path,
            node_is_character_device: metadata.file_type().is_char_device(),
            node_rdev: metadata.rdev(),
            udev_sysname,
            udev_rdev,
            stable_device_path,
        },
        sysfs_entry_names,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        io::Read,
        os::unix::net::UnixStream,
    };

    const DEVICE: &str = "/dev/dri/card0";
    const CONNECTOR: &str = "eDP-1";
    const STABLE_DEVICE: &str = "/sys/devices/pci0000:00/0000:00:02.0";
    const SYSFS_CARD: &str = "/sys/devices/pci0000:00/0000:00:02.0/drm/card0";
    const TEST_NONCE: [u8; CONFIRMATION_NONCE_BYTES] = [0x5a; CONFIRMATION_NONCE_BYTES];
    const INTENT: &str = "About to take DRM master of /dev/dri/card0 (eDP-1) on tty3 with requested scale 1; the physical mode will be selected after confirmation.";
    const CODE: &str = "5a5a5a5a";

    fn pump_key() -> super::super::kms::OutputKey {
        super::super::kms::OutputKey {
            device: 226,
            connector_name: "Offline-1".into(),
        }
    }

    fn selected_output_for_test(connector_id: u32) -> SelectedOutput {
        let mode = prior_mode_for_test();
        SelectedOutput {
            key: pump_key(),
            connector_id,
            connector_mode: mode,
            display: super::super::kms::AtomicOutputSelection {
                connector_id,
                crtc_id: connector_id.saturating_add(100),
                primary_plane_id: connector_id.saturating_add(200),
                mode,
                format: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
            },
            output_scale: OutputScale120::ONE,
            logical_rect: super::super::kms::LogicalRect {
                x: 0,
                y: 0,
                width: i32::try_from(mode.width).expect("test width fits"),
                height: i32::try_from(mode.height).expect("test height fits"),
            },
        }
    }

    #[test]
    fn adapter_start_clones_without_consuming_retained_selected_output() {
        let retained = Some(selected_output_for_test(41));

        let started = selected_output_for_adapter_start(&retained)
            .expect("adapter start receives the selected output");

        assert_eq!(started.connector_id, 41);
        assert_eq!(
            retained.as_ref().map(|output| output.connector_id),
            Some(41)
        );
    }

    #[test]
    fn successful_resume_add_and_change_replace_retained_connector_binding() {
        let mut retained = Some(selected_output_for_test(41));
        let added = selected_output_for_test(42);
        let add_commands = [KmsRenderCommand::AddOutput {
            generation: 4,
            output: added.clone(),
        }];

        refresh_selected_output_after_resume(
            &mut retained,
            resumed_selected_outputs(&add_commands),
            4,
        )
        .expect("successful AddOutput refreshes the retained binding");
        assert_eq!(retained, Some(added));

        let changed = selected_output_for_test(43);
        let change_commands = [KmsRenderCommand::ChangeOutput {
            generation: 6,
            output: changed.clone(),
        }];

        refresh_selected_output_after_resume(
            &mut retained,
            resumed_selected_outputs(&change_commands),
            6,
        )
        .expect("successful ChangeOutput refreshes the retained binding");
        assert_eq!(retained, Some(changed));
    }

    #[test]
    fn refresh_selects_the_output_command_matching_the_ready_generation() {
        let mut retained = Some(selected_output_for_test(41));
        let first = selected_output_for_test(42);
        let second = selected_output_for_test(43);
        let commands = [
            KmsRenderCommand::AddOutput {
                generation: 4,
                output: first,
            },
            KmsRenderCommand::AddOutput {
                generation: 6,
                output: second.clone(),
            },
        ];

        refresh_selected_output_after_resume(&mut retained, resumed_selected_outputs(&commands), 6)
            .expect("the ready generation selects its own command, not the first");
        assert_eq!(retained, Some(second));
    }

    #[test]
    fn refresh_refuses_a_ready_generation_no_output_command_carries() {
        let mut retained = Some(selected_output_for_test(41));
        let commands = [KmsRenderCommand::AddOutput {
            generation: 4,
            output: selected_output_for_test(42),
        }];

        let error = refresh_selected_output_after_resume(
            &mut retained,
            resumed_selected_outputs(&commands),
            9,
        )
        .expect_err("a ready generation without a matching output command is refused");
        assert!(error.to_string().contains("ready generation 9"));
        assert_eq!(
            retained.as_ref().map(|output| output.connector_id),
            Some(41),
            "the retained binding is untouched on refusal"
        );
    }

    #[test]
    fn refresh_accepts_a_replacement_connector_with_a_new_key() {
        let mut retained = Some(selected_output_for_test(41));
        let mut foreign = selected_output_for_test(42);
        foreign.key.connector_name = "HDMI-A-9".into();
        let commands = [KmsRenderCommand::AddOutput {
            generation: 4,
            output: foreign,
        }];

        refresh_selected_output_after_resume(&mut retained, resumed_selected_outputs(&commands), 4)
            .expect("a replacement output becomes the retained resume binding");
        assert_eq!(
            retained
                .as_ref()
                .map(|output| (output.key.connector_name.clone(), output.connector_id)),
            Some(("HDMI-A-9".to_string(), 42)),
            "the replacement binding becomes authoritative"
        );
    }

    fn submitted_event() -> KmsRenderFrameEvent {
        KmsRenderFrameEvent::FrameSubmitted {
            generation: 1,
            key: pump_key(),
            frame_token: 1,
            timestamp: super::super::render::KmsPresentationTimestamp {
                seconds: 1,
                nanoseconds: 2,
            },
            security_epochs: Vec::new(),
        }
    }

    fn presentation_cancelled_event(generation: u64) -> KmsRenderFrameEvent {
        KmsRenderFrameEvent::PresentationCancelled {
            generation,
            key: pump_key(),
        }
    }

    fn atomic_commit_failure(errno: i32) -> KmsRenderFrameEvent {
        atomic_commit_failure_for_generation(errno, 1)
    }

    fn atomic_commit_failure_for_generation(errno: i32, generation: u64) -> KmsRenderFrameEvent {
        KmsRenderFrameEvent::TerminalFailure(super::super::worker::KmsRenderWorkerFailure {
            operation: super::super::kms::KmsRenderOperation::Worker,
            generation,
            key: Some(pump_key()),
            failure: super::super::worker::KmsRenderPlatformFailure::terminal(
                "kms-live-atomic-commit-hard-rejection",
                format!("atomic commit ioctl failed with errno {errno}: injected commit outcome"),
            ),
        })
    }

    fn observed_pause_acknowledgement()
    -> (ExternalPauseAcknowledgement, mpsc::Receiver<&'static str>) {
        let (observed, receiver) = mpsc::channel();
        (
            ExternalPauseAcknowledgement::new(move || observed.send("acknowledged").is_ok()),
            receiver,
        )
    }

    struct SupervisorMailbox {
        polls: VecDeque<Option<LiveCoordinatorEvent>>,
        waits: VecDeque<Option<LiveCoordinatorEvent>>,
        advances: VecDeque<Duration>,
        waited_for: Vec<Duration>,
        clock: Rc<Cell<Duration>>,
    }

    impl SupervisorMailbox {
        fn new(
            waits: impl IntoIterator<Item = Option<LiveCoordinatorEvent>>,
            polls: impl IntoIterator<Item = Option<LiveCoordinatorEvent>>,
        ) -> Self {
            Self {
                polls: polls.into_iter().collect(),
                waits: waits.into_iter().collect(),
                advances: VecDeque::new(),
                waited_for: Vec::new(),
                clock: Rc::new(Cell::new(Duration::ZERO)),
            }
        }

        fn now(&self) -> impl FnMut() -> Duration + use<> {
            let clock = Rc::clone(&self.clock);
            move || clock.get()
        }
    }

    impl LiveCoordinatorMailbox for SupervisorMailbox {
        fn poll_event(&mut self) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
            Ok(self.polls.pop_front().flatten())
        }

        fn wait_for_event_timeout(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<LiveCoordinatorEvent>, KmsLiveError> {
            self.waited_for.push(timeout);
            let event = self.waits.pop_front().unwrap_or(None);
            let advance = self.advances.pop_front().unwrap_or_else(|| {
                if event.is_some() {
                    Duration::ZERO
                } else {
                    timeout
                }
            });
            self.clock.set(self.clock.get().saturating_add(advance));
            Ok(event)
        }
    }

    struct SupervisorPreparation {
        statuses: VecDeque<super::super::render::LiveRenderPreparationStatus>,
        waited_for: Vec<Duration>,
    }

    impl LivePumpPreparationControl for SupervisorPreparation {
        fn wait_slice(
            &mut self,
            timeout: Duration,
        ) -> Result<super::super::render::LiveRenderPreparationStatus, KmsLiveError> {
            self.waited_for.push(timeout);
            Ok(self
                .statuses
                .pop_front()
                .unwrap_or(super::super::render::LiveRenderPreparationStatus::Pending))
        }
    }

    #[derive(Default)]
    struct SupervisorPump {
        commands: Vec<&'static str>,
        stops: usize,
        nominal: Duration,
    }

    impl SupervisorPump {
        fn at_60_hz() -> Self {
            Self {
                nominal: Duration::from_millis(16),
                ..Self::default()
            }
        }
    }

    impl LivePumpControl for SupervisorPump {
        fn request_registration(&mut self) -> Result<(), KmsLiveError> {
            self.commands.push("registration");
            Ok(())
        }

        fn request_update(&mut self) -> Result<(), KmsLiveError> {
            self.commands.push("update");
            Ok(())
        }

        fn begin_stop(&mut self) {
            self.stops = self.stops.saturating_add(1);
        }

        fn nominal_refresh_interval(&self) -> Duration {
            self.nominal
        }

        fn begin_transition(
            &mut self,
            _commands: Vec<KmsRenderCommand>,
        ) -> Result<(), KmsLiveError> {
            self.commands.push("begin-transition");
            Ok(())
        }

        fn stage_resume_lease(
            &mut self,
            _generation: u64,
            resume: super::super::render::StagedResumeLease,
        ) -> Result<(), KmsLiveError> {
            drop(resume);
            self.commands.push("stage-resume-lease");
            Ok(())
        }

        fn transition_update(&mut self, _generation: u64) -> Result<(), KmsLiveError> {
            self.commands.push("transition-update");
            Ok(())
        }

        fn drain_scene(&mut self, _generation: u64) -> Result<(), KmsLiveError> {
            self.commands.push("drain-scene");
            Ok(())
        }
    }

    struct ReleaseOrderingPump {
        order: Rc<RefCell<Vec<&'static str>>>,
        pairing: LiveTargetPairingLedger,
        target_generation: u64,
    }

    impl LivePumpControl for ReleaseOrderingPump {
        fn request_registration(&mut self) -> Result<(), KmsLiveError> {
            unreachable!("release-order test enters at Suspend")
        }

        fn request_update(&mut self) -> Result<(), KmsLiveError> {
            unreachable!("release-order test enters at Suspend")
        }

        fn begin_stop(&mut self) {}

        fn nominal_refresh_interval(&self) -> Duration {
            Duration::from_millis(16)
        }

        fn begin_transition(
            &mut self,
            commands: Vec<KmsRenderCommand>,
        ) -> Result<(), KmsLiveError> {
            assert!(matches!(
                commands.as_slice(),
                [KmsRenderCommand::Suspend { generation: 2 }]
            ));
            Ok(())
        }

        fn transition_update(&mut self, generation: u64) -> Result<(), KmsLiveError> {
            assert_eq!(generation, 2);
            self.pairing.record_released(self.target_generation);
            self.order.borrow_mut().push("atomic-target-release");
            Ok(())
        }
    }

    fn pump_reply(reply: PumpReply) -> Option<LiveCoordinatorEvent> {
        Some(LiveCoordinatorEvent::Pump(reply))
    }

    fn started_reply() -> Option<LiveCoordinatorEvent> {
        pump_reply(PumpReply::Started(Ok(())))
    }

    fn ready_reply() -> Option<LiveCoordinatorEvent> {
        pump_reply(PumpReply::Registration(Ok(LiveOutputRegistration::Ready)))
    }

    fn updated_reply(events: Vec<KmsRenderFrameEvent>) -> Option<LiveCoordinatorEvent> {
        pump_reply(PumpReply::Updated(Ok(events)))
    }

    #[test]
    fn initial_active_boundary_is_reported_only_after_output_ready() {
        let observed = Cell::new(0_u32);
        let mut mailbox = SupervisorMailbox::new(
            [started_reply(), ready_reply()],
            [
                None,
                None,
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::TargetHotplug,
                )),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let end = supervise_active_live_operation_after_output_ready(
            &mut mailbox,
            &mut pump,
            now,
            |_| observed.set(observed.get().saturating_add(1)),
            || Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("output readiness is followed by the queued revocation");
        assert_eq!(
            end,
            ActiveLiveOperationEnd::Revocation {
                revocation: LiveRevocation::TargetHotplug,
                teardown: SessionTeardown::Graceful,
            }
        );
        assert_eq!(observed.get(), 1);

        let observed = Cell::new(0_u32);
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                Some(LiveCoordinatorEvent::VtSwitchRequested(4)),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let end = supervise_active_live_operation_after_output_ready(
            &mut mailbox,
            &mut pump,
            now,
            |_| observed.set(observed.get().saturating_add(1)),
            || Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("the pre-ready switch remains resumable");
        assert_eq!(
            end,
            ActiveLiveOperationEnd::VtSwitchRequested {
                vt: 4,
                outstanding_command: Some(OutstandingPumpCommand::Registration),
            }
        );
        assert_eq!(observed.get(), 0);
    }

    #[test]
    fn pre_ready_pause_resume_captures_first_output_ready_fd_baseline() {
        let (acknowledgement, _acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let mut baseline = LiveActiveFdBaseline::default();

        let end = supervise_active_live_operation_after_output_ready(
            &mut mailbox,
            &mut pump,
            now,
            |_| {
                let _ = baseline.observe_output_ready(Some(40));
            },
            || Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("the pre-ready pause remains resumable");
        assert!(matches!(
            end,
            ActiveLiveOperationEnd::PauseRequested {
                generation: 2,
                outstanding_command: Some(OutstandingPumpCommand::Registration),
                ..
            }
        ));
        assert_eq!(baseline, LiveActiveFdBaseline::default());

        let mut mailbox = SupervisorMailbox::new(
            [
                pump_reply(PumpReply::ResumeLeaseStaged {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::OutputReady {
                        generation: 2,
                        key: pump_key(),
                    }]),
                }),
            ],
            [],
        );
        let mut now = mailbox.now();
        let outcome = drive_live_transition(
            &mut mailbox,
            &mut pump,
            vec![KmsRenderCommand::Resume { generation: 2 }],
            Some((
                2,
                super::super::render::staged_resume_lease_for_test(MasterDrmLease {
                    fd: harmless_fd(),
                }),
            )),
            Duration::from_secs(30),
            &mut now,
        )
        .expect("the resumed generation reaches its first OutputReady");
        assert_eq!(
            outcome,
            LiveTransitionOutcome::OutputReady { generation: 2 }
        );

        let active = baseline.observe_output_ready(Some(40));
        assert_eq!(
            active,
            LiveFdTelemetry {
                fd_count: Some(40),
                fd_delta: Some(0),
                first_output_ready: true,
            },
            "the first resumed OutputReady emits the cycle-zero Active telemetry"
        );
        assert_eq!(baseline.fd_count, Some(40));

        let later = baseline.observe_output_ready(Some(43));
        assert_eq!(later.fd_delta, Some(3));
        assert!(!later.first_output_ready, "the first baseline wins");
        assert_eq!(baseline.fd_count, Some(40));
    }

    #[test]
    fn resume_transition_failure_is_followed_by_a_confirmed_suspend_before_retry() {
        let mut mailbox = SupervisorMailbox::new(
            [
                pump_reply(PumpReply::ResumeLeaseStaged {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::OutputFailed {
                        generation: 2,
                        key: pump_key(),
                        reason: "kms-live-test-output-not-ready: not ready".into(),
                    }]),
                }),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 3,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 3,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 3 }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let mut now = mailbox.now();
        let resume = drive_live_transition(
            &mut mailbox,
            &mut pump,
            vec![KmsRenderCommand::Resume { generation: 2 }],
            Some((
                2,
                super::super::render::staged_resume_lease_for_test(MasterDrmLease {
                    fd: harmless_fd(),
                }),
            )),
            Duration::from_secs(30),
            &mut now,
        )
        .expect("failure-atomic resume attempt answers");
        assert!(matches!(
            resume,
            LiveTransitionOutcome::OutputFailed { generation: 2, .. }
        ));
        assert_eq!(
            transition_resume_generation(&[KmsRenderCommand::Resume { generation: 2 }]),
            Some(2)
        );
        let rollback = drive_live_transition(
            &mut mailbox,
            &mut pump,
            vec![KmsRenderCommand::Suspend { generation: 3 }],
            None,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("failed attempt is explicitly suspended");
        assert_eq!(rollback, LiveTransitionOutcome::Suspended { generation: 3 });
        assert_eq!(
            pump.commands,
            [
                "stage-resume-lease",
                "begin-transition",
                "transition-update",
                "begin-transition",
                "transition-update",
            ]
        );
    }

    #[test]
    fn chord_with_an_update_in_flight_reconciles_before_suspend_transition() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                Some(LiveCoordinatorEvent::VtSwitchRequested(4)),
                updated_reply(vec![submitted_event()]),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 2 }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let end = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect("the in-flight update is handed to pause reconciliation");
        assert_eq!(
            end,
            ActiveLiveOperationEnd::VtSwitchRequested {
                vt: 4,
                outstanding_command: Some(OutstandingPumpCommand::Update),
            }
        );

        let mut now = mailbox.now();
        reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::SelfSwitch,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("the pending Updated reply is drained before transition commands");
        let outcome = drive_live_transition(
            &mut mailbox,
            &mut pump,
            vec![KmsRenderCommand::Suspend { generation: 2 }],
            None,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("the capacity-one pump accepts the pause transition deterministically");
        assert_eq!(outcome, LiveTransitionOutcome::Suspended { generation: 2 });
        assert_eq!(
            pump.commands,
            [
                "registration",
                "update",
                "begin-transition",
                "transition-update",
            ]
        );
    }

    #[test]
    fn suspend_transition_waits_for_recorded_target_release() {
        let pairing = LiveTargetPairingLedger::default();
        pairing.record_created(1);
        let order = Rc::new(RefCell::new(Vec::new()));
        let mut pump = ReleaseOrderingPump {
            order: Rc::clone(&order),
            pairing: pairing.clone(),
            target_generation: 1,
        };
        let mut mailbox = SupervisorMailbox::new(
            [
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 2 }]),
                }),
            ],
            [],
        );
        let mut now = mailbox.now();

        assert_eq!(
            drive_live_transition(
                &mut mailbox,
                &mut pump,
                vec![KmsRenderCommand::Suspend { generation: 2 }],
                None,
                Duration::from_secs(30),
                &mut now,
            )
            .expect("Suspended is published only after atomic target release"),
            LiveTransitionOutcome::Suspended { generation: 2 }
        );
        assert!(
            pairing.snapshot(1).is_paired(),
            "the suspend transition returns only after the pump records target release"
        );
        assert_eq!(order.borrow().as_slice(), ["atomic-target-release"]);
    }

    #[test]
    fn chord_during_registration_reconciles_before_suspend_transition() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                Some(LiveCoordinatorEvent::VtSwitchRequested(4)),
                ready_reply(),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 2 }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let end = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect("registration chord is handed to pause reconciliation");
        assert_eq!(
            end,
            ActiveLiveOperationEnd::VtSwitchRequested {
                vt: 4,
                outstanding_command: Some(OutstandingPumpCommand::Registration),
            }
        );

        let mut now = mailbox.now();
        reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Registration,
            LivePauseCause::SelfSwitch,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("the pending Registration reply is drained before transition commands");
        let outcome = drive_live_transition(
            &mut mailbox,
            &mut pump,
            vec![KmsRenderCommand::Suspend { generation: 2 }],
            None,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("registration reconciliation frees the capacity-one command slot");
        assert_eq!(outcome, LiveTransitionOutcome::Suspended { generation: 2 });
        assert_eq!(
            pump.commands,
            ["registration", "begin-transition", "transition-update"]
        );
    }

    #[test]
    fn external_pause_reconciles_late_eacces_commit_failure_and_reaches_paused() {
        let (acknowledgement, _acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                // Authority was revoked and the pause publication reaches the
                // session thread only after the presenter's post-ioctl sample.
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
                updated_reply(vec![atomic_commit_failure(libc::EACCES)]),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 2 }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let end = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect("external pause claims the in-flight update");
        let ActiveLiveOperationEnd::PauseRequested {
            generation,
            outstanding_command,
            ..
        } = end
        else {
            panic!("external pause ended active supervision as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, Some(OutstandingPumpCommand::Update));

        let mut lifecycle = LiveCoordinatorLifecycle::active(1, Duration::ZERO);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation })
                .expect("pause begins"),
            LiveCoordinatorLifecycleAction::BeginPause
        );
        let mut now = mailbox.now();
        reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::External,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("authority-class atomic failure is attributable to established pause");
        assert_eq!(
            drive_live_transition(
                &mut mailbox,
                &mut pump,
                vec![KmsRenderCommand::Suspend { generation }],
                None,
                Duration::from_secs(30),
                &mut now,
            )
            .expect("suspend follows reconciled cancelled update"),
            LiveTransitionOutcome::Suspended { generation }
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation })
                .expect("paused boundary is reached"),
            LiveCoordinatorLifecycleAction::Paused
        );
    }

    #[test]
    fn formerly_worker_fatal_authority_failure_reaches_paused_and_resumes() {
        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                // The completed update beats the session callback: this is
                // the cycle-21 banked-gate ordering.
                updated_reply(vec![atomic_commit_failure_for_generation(libc::EACCES, 61)]),
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 62,
                    acknowledgement,
                }),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 62,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 62,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 62 }]),
                }),
                pump_reply(PumpReply::ResumeLeaseStaged {
                    generation: 63,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 64,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 64,
                    result: Ok(vec![KmsRenderReply::OutputReady {
                        generation: 64,
                        key: pump_key(),
                    }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let end = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 61,
            },
            LiveSceneMode::FirstLight,
            now,
            |_| Ok(crate::protocol::EventFlushOutcome::Complete),
            || Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("late external pause attributes the reply-carried authority failure");
        let LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } = end
        else {
            panic!("authority-failure arbitration ended as {end:?}");
        };
        assert_eq!(generation, 62);
        assert_eq!(
            outstanding_command, None,
            "the failed update completed before pause attribution"
        );

        let mut authority = LiveSessionAuthority::Active { generation: 61 };
        assert_eq!(
            authority.request_pause().expect("external pause begins"),
            LivePauseRequestDisposition::External { generation: 62 }
        );
        let mut lifecycle = LiveCoordinatorLifecycle::active(61, Duration::ZERO);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation })
                .expect("attributed pause begins"),
            LiveCoordinatorLifecycleAction::BeginPause
        );
        let mut now = mailbox.now();
        assert_eq!(
            drive_live_transition(
                &mut mailbox,
                &mut pump,
                vec![KmsRenderCommand::Suspend { generation }],
                None,
                Duration::from_secs(30),
                &mut now,
            )
            .expect("the live worker remains available for Suspend"),
            LiveTransitionOutcome::Suspended { generation }
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation })
                .expect("the attributed failure reaches Paused"),
            LiveCoordinatorLifecycleAction::Paused
        );
        assert_eq!(
            authority.complete_pause(true),
            Some(LivePauseCompletion {
                generation,
                cause: LivePauseCause::External,
                resumable: true,
                activate_pending: false,
            })
        );
        assert!(acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");

        let resume_generation = authority.begin_resume().expect("pause resumes");
        assert_eq!(resume_generation, 63);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginResume {
                    generation: resume_generation,
                })
                .expect("lifecycle begins resume"),
            LiveCoordinatorLifecycleAction::BeginResume
        );
        assert_eq!(
            drive_live_transition(
                &mut mailbox,
                &mut pump,
                vec![
                    KmsRenderCommand::Resume {
                        generation: resume_generation,
                    },
                    KmsRenderCommand::AddOutput {
                        generation: 64,
                        output: selected_output_for_test(42),
                    },
                ],
                Some((
                    resume_generation,
                    super::super::render::staged_resume_lease_for_test(MasterDrmLease {
                        fd: harmless_fd(),
                    }),
                )),
                Duration::from_secs(30),
                &mut now,
            )
            .expect("fresh staged authority rebuilds the output"),
            LiveTransitionOutcome::OutputReady { generation: 64 }
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::OutputReady {
                    generation: 64,
                    observed_at: now(),
                })
                .expect("resumed output returns Active"),
            LiveCoordinatorLifecycleAction::Active
        );
    }

    #[test]
    fn reply_carried_authority_failure_without_pause_remains_terminal() {
        let mut mailbox = SupervisorMailbox::new(
            [
                updated_reply(vec![atomic_commit_failure_for_generation(libc::EACCES, 61)]),
                None,
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 61,
            },
            LiveSceneMode::FirstLight,
            now,
            |_| Ok(crate::protocol::EventFlushOutcome::Complete),
            || Ok(()),
            |_, _, _| Ok(()),
        )
        .expect_err("authority failure without an external pause remains terminal");
        assert!(
            error
                .to_string()
                .contains("kms-live-atomic-commit-hard-rejection")
        );
        assert!(
            error
                .to_string()
                .contains(&format!("errno {}", libc::EACCES))
        );
        assert_eq!(mailbox.clock.get(), NO_SUBMIT_TIMEOUT);
        assert_eq!(pump.commands, ["update"]);
    }

    #[test]
    fn external_pause_racing_pageflip_reaches_paused_boundary() {
        let (acknowledgement, _acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
                updated_reply(vec![presentation_cancelled_event(1)]),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
                pump_reply(PumpReply::TransitionUpdated {
                    generation: 2,
                    result: Ok(vec![KmsRenderReply::Suspended { generation: 2 }]),
                }),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        let end = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect("external pause claims the in-flight pageflip update");
        let ActiveLiveOperationEnd::PauseRequested {
            generation,
            outstanding_command,
            ..
        } = end
        else {
            panic!("external pause ended active supervision as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, Some(OutstandingPumpCommand::Update));

        let mut lifecycle = LiveCoordinatorLifecycle::active(1, Duration::ZERO);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation })
                .expect("pageflip race begins its pause"),
            LiveCoordinatorLifecycleAction::BeginPause
        );
        let mut now = mailbox.now();
        reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::External,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("cancelled pageflip update reconciles without a submitted frame");
        assert_eq!(
            drive_live_transition(
                &mut mailbox,
                &mut pump,
                vec![KmsRenderCommand::Suspend { generation }],
                None,
                Duration::from_secs(30),
                &mut now,
            )
            .expect("suspend follows the cancelled pageflip"),
            LiveTransitionOutcome::Suspended { generation }
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation })
                .expect("pageflip race reaches the paused boundary"),
            LiveCoordinatorLifecycleAction::Paused
        );
    }

    #[test]
    fn cancelled_update_reconciles_as_no_submitted_frame() {
        let mut mailbox =
            SupervisorMailbox::new([updated_reply(vec![presentation_cancelled_event(17)])], []);
        let mut now = mailbox.now();

        reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::External,
            Duration::from_secs(30),
            &mut now,
        )
        .expect("typed cancellation is an empty successful reconciliation");
    }

    #[test]
    fn authority_commit_failure_during_pure_self_switch_remains_terminal_and_named() {
        let mut mailbox = SupervisorMailbox::new(
            [updated_reply(vec![atomic_commit_failure(libc::EACCES)])],
            [],
        );
        let mut now = mailbox.now();

        let error = reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::SelfSwitch,
            Duration::from_secs(30),
            &mut now,
        )
        .expect_err("pure self-switch still owns DRM authority");
        assert!(matches!(error, KmsLiveError::TerminalFrame(_)));
        assert!(
            error
                .to_string()
                .contains("kms-live-atomic-commit-hard-rejection")
        );
        assert!(
            error
                .to_string()
                .contains(&format!("errno {}", libc::EACCES))
        );
    }

    #[test]
    fn authority_commit_failure_during_self_switch_racing_external_pause_is_demoted() {
        let (acknowledgement, _acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
                updated_reply(vec![atomic_commit_failure(libc::EACCES)]),
            ],
            [],
        );
        let mut now = mailbox.now();
        let mut external_pause = None;

        {
            let mut collecting_mailbox =
                PauseCollectingMailbox::new(&mut mailbox, &mut external_pause);
            reconcile_outstanding_pump_command(
                &mut collecting_mailbox,
                OutstandingPumpCommand::Update,
                LivePauseCause::SelfSwitch,
                Duration::from_secs(30),
                &mut now,
            )
            .expect("a collected racing external pause proves authority revocation");
        }
        assert_eq!(
            external_pause.as_ref().map(|pause| pause.generation),
            Some(2)
        );
    }

    #[test]
    fn authority_commit_failure_outside_pause_remains_terminal_and_named() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                updated_reply(vec![atomic_commit_failure(libc::EACCES)]),
            ],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect_err("authority failure outside pause is never demoted");
        assert!(
            error
                .to_string()
                .contains("kms-live-atomic-commit-hard-rejection")
        );
        assert!(
            error
                .to_string()
                .contains(&format!("errno {}", libc::EACCES))
        );
    }

    #[test]
    fn non_authority_commit_failure_during_pause_remains_terminal() {
        let mut mailbox = SupervisorMailbox::new(
            [updated_reply(vec![atomic_commit_failure(libc::EINVAL)])],
            [],
        );
        let mut now = mailbox.now();

        let error = reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::External,
            Duration::from_secs(30),
            &mut now,
        )
        .expect_err("EINVAL is independent evidence of a broken commit path");
        assert!(matches!(error, KmsLiveError::TerminalFrame(_)));
        assert!(
            error
                .to_string()
                .contains(&format!("errno {}", libc::EINVAL))
        );
    }

    #[test]
    fn worker_failure_in_pause_reconciliation_still_outranks_the_pause() {
        let failure =
            KmsRenderFrameEvent::TerminalFailure(super::super::worker::KmsRenderWorkerFailure {
                operation: super::super::kms::KmsRenderOperation::Worker,
                generation: 1,
                key: Some(pump_key()),
                failure: super::super::worker::KmsRenderPlatformFailure::terminal(
                    "injected-pause-window-worker-failure",
                    "the worker failed independently of authority revocation",
                ),
            });
        let mut mailbox = SupervisorMailbox::new([updated_reply(vec![failure])], []);
        let mut now = mailbox.now();

        let error = reconcile_outstanding_pump_command(
            &mut mailbox,
            OutstandingPumpCommand::Update,
            LivePauseCause::External,
            Duration::from_secs(30),
            &mut now,
        )
        .expect_err("worker failure remains terminal during pause reconciliation");
        assert!(matches!(error, KmsLiveError::TerminalFrame(_)));
        assert!(
            error
                .to_string()
                .contains("injected-pause-window-worker-failure")
        );
    }

    #[test]
    fn queued_transition_reply_at_the_exact_deadline_wins() {
        let mut mailbox = SupervisorMailbox::new(
            [],
            [pump_reply(PumpReply::TransitionBegun {
                generation: 2,
                result: Ok(()),
            })],
        );
        let deadline = Duration::from_secs(30);
        mailbox.clock.set(deadline);
        let mut now = mailbox.now();

        assert!(matches!(
            wait_for_transition_reply(&mut mailbox, deadline, &mut now, "boundary"),
            Ok(PumpReply::TransitionBegun { generation: 2, .. })
        ));
        assert!(mailbox.waited_for.is_empty(), "the queued reply is polled");
    }

    #[test]
    fn every_resume_stage_is_capped_by_one_overall_deadline() {
        let deadline = Duration::from_secs(30);
        assert_eq!(
            remaining_resume_stage_timeout(
                deadline,
                Duration::ZERO,
                RUNNING_SESSION_COMMAND_TIMEOUT,
            )
            .expect("DRM open has its warm running-session bound"),
            Duration::from_secs(3)
        );
        assert_eq!(
            remaining_resume_stage_timeout(
                deadline,
                Duration::from_secs(15),
                LIVE_INPUT_LIFECYCLE_TIMEOUT,
            )
            .expect("input resume has its ordinary bound"),
            Duration::from_secs(5)
        );
        assert_eq!(
            remaining_resume_stage_timeout(
                deadline,
                Duration::from_secs(28),
                RUNNING_SESSION_COMMAND_TIMEOUT,
            )
            .expect("lease duplication is clipped to the remaining budget"),
            Duration::from_secs(2)
        );
        assert!(
            remaining_resume_stage_timeout(
                deadline,
                Duration::from_secs(30),
                RUNNING_SESSION_COMMAND_TIMEOUT,
            )
            .expect_err("no later stage may extend the overall attempt")
            .to_string()
            .contains("30s overall deadline")
        );
    }

    #[test]
    fn exhausted_resume_budget_refuses_a_synchronous_stage_before_entry() {
        let entered = Cell::new(false);
        let deadline = Duration::from_secs(30);
        let error =
            run_resume_synchronous_stage(deadline, deadline, "Vulkan display probe", || {
                entered.set(true);
                Ok(())
            })
            .expect_err("an exhausted budget refuses the next synchronous stage");

        assert!(!entered.get(), "the synchronous probe must not be entered");
        assert!(error.to_string().contains("before Vulkan display probe"));
    }

    #[test]
    fn every_update_pulses_zero_or_one_times_for_zero_one_or_many_submissions() {
        assert!(require_resumed_frame_generation(4, 4).is_ok());
        assert!(
            require_resumed_frame_generation(4, 1)
                .expect_err("a queued pre-pause submission is stale after resume")
                .to_string()
                .starts_with("kms-live-stale-generation:")
        );
        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [
                updated_reply(Vec::new()),
                updated_reply(vec![KmsRenderFrameEvent::FrameSubmitted {
                    generation: 1,
                    key: pump_key(),
                    frame_token: 1,
                    timestamp: super::super::render::KmsPresentationTimestamp {
                        seconds: 1,
                        nanoseconds: 2,
                    },
                    security_epochs: vec![51],
                }]),
                updated_reply(vec![
                    submitted_event(),
                    submitted_event(),
                    submitted_event(),
                ]),
            ],
            [
                None,
                None,
                None,
                None,
                None,
                None,
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let (frame_clock, pulse_probe) = crate::protocol::ClientFrameClock::test_channel();
        let flushes = Cell::new(0_u32);
        let displayed_security = RefCell::new(Vec::new());
        let now = mailbox.now();
        let end = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 1,
            },
            LiveSceneMode::ClientContent,
            now,
            |_| {
                flushes.set(flushes.get() + 1);
                Ok(crate::protocol::EventFlushOutcome::Complete)
            },
            || frame_clock.pulse().map_err(KmsLiveError::Setup),
            |epoch, generation, output| {
                displayed_security
                    .borrow_mut()
                    .push((epoch, generation, output));
                Ok(())
            },
        )
        .expect("resumed client-scene supervision reaches the next terminal event");
        let LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } = end
        else {
            panic!("resumed supervision ended as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, None);
        assert!(acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
        assert_eq!(
            flushes.get(),
            1,
            "resume flushes once before update pumping"
        );
        assert_eq!(
            pulse_probe.drain(),
            2,
            "zero, one and many submissions produce zero, one and one real client-clock pulses"
        );
        assert_eq!(pump.commands, ["update", "update", "update"]);
        assert_eq!(
            *displayed_security.borrow(),
            [(51, 1, pump_key())],
            "only a displayed frame carries its security epoch to protocol"
        );
    }

    #[test]
    fn first_light_resume_skips_flush_when_runtime_owned_scene_channel_is_saturated() {
        let (scene_sender, _runtime_owned_feed) = crate::protocol::ClientSceneFeed::test_channel();
        scene_sender.send(Vec::new()).expect("fill event slot A");
        scene_sender.send(Vec::new()).expect("fill event slot B");
        assert!(matches!(
            scene_sender.try_send(Vec::new()),
            Err(mpsc::TrySendError::Full(_))
        ));

        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [updated_reply(vec![submitted_event()])],
            [
                None,
                None,
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let (frame_clock, pulse_probe) = crate::protocol::ClientFrameClock::test_channel();
        let flushes = Cell::new(0_u32);
        let now = mailbox.now();

        let end = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 1,
            },
            LiveSceneMode::FirstLight,
            now,
            |_| {
                flushes.set(flushes.get().saturating_add(1));
                Ok(crate::protocol::EventFlushOutcome::Pending)
            },
            || frame_clock.pulse().map_err(KmsLiveError::Setup),
            |_, _, _| Ok(()),
        )
        .expect("first-light resumes without entering the client-scene flush loop");

        let LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } = end
        else {
            panic!("first-light resume supervision ended as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, None);
        assert!(acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
        assert_eq!(flushes.get(), 0, "first-light has no scene flush stage");
        assert_eq!(pulse_probe.drain(), 1);
        assert_eq!(pump.commands, ["update"]);
    }

    #[test]
    fn resume_flush_uses_production_drain_only_path_before_rendering_newest_state() {
        struct SceneDrainPump {
            app: bevy::app::App,
            commands: Vec<&'static str>,
            rendered_markers: Rc<RefCell<Vec<u8>>>,
        }

        impl LivePumpControl for SceneDrainPump {
            fn request_registration(&mut self) -> Result<(), KmsLiveError> {
                unreachable!("the resumed path is already registered")
            }

            fn request_update(&mut self) -> Result<(), KmsLiveError> {
                self.commands.push("update");
                self.app.update();
                let marker = self
                    .app
                    .world()
                    .resource::<bevy::asset::Assets<bevy::image::Image>>()
                    .iter()
                    .find_map(|(_, image)| image.data.as_ref()?.first().copied())
                    .expect("the first rendered update sees a client surface");
                self.rendered_markers.borrow_mut().push(marker);
                Ok(())
            }

            fn begin_stop(&mut self) {}

            fn nominal_refresh_interval(&self) -> Duration {
                Duration::from_millis(16)
            }

            fn drain_scene(&mut self, _generation: u64) -> Result<(), KmsLiveError> {
                self.commands.push("drain-scene");
                super::super::render::drain_live_client_scene(&mut self.app);
                Ok(())
            }
        }

        let (scene_sender, scene_feed) = crate::protocol::ClientSceneFeed::test_channel();
        let app =
            super::super::render::tests::live_client_scene_app_for_test(scene_feed, (320, 240));
        let rendered_markers = Rc::new(RefCell::new(Vec::new()));
        let mut pump = SceneDrainPump {
            app,
            commands: Vec::new(),
            rendered_markers: Rc::clone(&rendered_markers),
        };
        let surface = crate::protocol::SurfaceId(1);
        let layout = crate::protocol::SurfaceLayout {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            z: crate::protocol::SurfaceStackKey::normal(0),
            source: None,
            parent: None,
            transform: crate::protocol::SurfaceTransform::Normal,
            visible: true,
            toplevel: None,
        };
        let batch = |marker| {
            vec![crate::protocol::ProtocolEvent::SurfaceUpserted {
                id: surface,
                scene: crate::protocol::SurfaceSceneSnapshot {
                    layout,
                    kind: crate::protocol::SceneSurfaceKind::Toplevel,
                    title: None,
                },
                frame: crate::protocol::SurfaceFrame::Shm(crate::protocol::ShmFrame {
                    width: 1,
                    height: 1,
                    opaque: true,
                    rgba: Arc::new(vec![marker, 0, 0, 0xff]),
                }),
            }]
        };
        scene_sender.send(batch(0x45)).expect("queue E");
        scene_sender.send(batch(0x46)).expect("queue F");

        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [],
            [
                pump_reply(PumpReply::SceneDrained {
                    generation: 1,
                    result: Ok(()),
                }),
                None,
                updated_reply(vec![submitted_event()]),
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
            ],
        );
        let flushes = Cell::new(0_u32);
        let timeouts = RefCell::new(Vec::new());
        let pulses = Cell::new(0_u32);
        let now = mailbox.now();

        let end = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 1,
            },
            LiveSceneMode::ClientContent,
            now,
            |timeout| {
                timeouts.borrow_mut().push(timeout);
                assert!(
                    rendered_markers.borrow().is_empty(),
                    "drain-only must not acquire, render or present before flush completion"
                );
                assert_eq!(
                    pulses.get(),
                    0,
                    "drain-only cannot manufacture a frame submission"
                );
                flushes.set(flushes.get().saturating_add(1));
                if flushes.get() == 1 {
                    Ok(crate::protocol::EventFlushOutcome::Pending)
                } else {
                    scene_sender.send(batch(0x47)).expect("publish newest G");
                    Ok(crate::protocol::EventFlushOutcome::Complete)
                }
            },
            || {
                pulses.set(pulses.get().saturating_add(1));
                Ok(())
            },
            |_, _, _| Ok(()),
        )
        .expect("the production resume loop drains before its first render");

        let LiveSupervisionEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } = end
        else {
            panic!("resume supervision ended as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, None);
        assert!(acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
        assert_eq!(pump.commands, ["drain-scene", "update"]);
        assert_eq!(
            *rendered_markers.borrow(),
            [0x47],
            "the first rendered frame reflects newest state G"
        );
        assert_eq!(pulses.get(), 1, "G's submission is pulsed normally");
        assert_eq!(
            *timeouts.borrow(),
            [LIVE_TOPOLOGY_ACK_TIMEOUT, LIVE_TOPOLOGY_ACK_TIMEOUT]
        );
    }

    #[test]
    fn sustained_resume_scene_refill_exhausts_the_no_submit_budget() {
        let mut mailbox = SupervisorMailbox::new(
            [pump_reply(PumpReply::SceneDrained {
                generation: 1,
                result: Ok(()),
            })],
            [None],
        );
        mailbox.advances.push_back(NO_SUBMIT_TIMEOUT);
        let mut pump = SupervisorPump::at_60_hz();
        let flushes = Cell::new(0_u32);
        let now = mailbox.now();

        let error = supervise_resumed_live_render(
            &mut mailbox,
            &mut pump,
            ResumedLiveOutput {
                ready_at: Duration::ZERO,
                generation: 1,
            },
            LiveSceneMode::ClientContent,
            now,
            |_| {
                flushes.set(flushes.get() + 1);
                Ok(crate::protocol::EventFlushOutcome::Pending)
            },
            || panic!("budget exhaustion occurs before active rendering"),
            |_, _, _| Ok(()),
        )
        .expect_err("continuous refill must terminate at the no-submit deadline");

        assert!(error.to_string().contains("submitted no frame for 2s"));
        assert_eq!(flushes.get(), 1);
        assert_eq!(pump.commands, ["drain-scene"]);
        assert_eq!(mailbox.waited_for, [NO_SUBMIT_TIMEOUT]);
    }

    #[test]
    fn signal_interrupts_sliced_render_preparation_before_readiness() {
        let mut mailbox = SupervisorMailbox::new(
            [],
            [
                None,
                Some(LiveCoordinatorEvent::Signal(LiveSignal::Terminate)),
            ],
        );
        let mut preparation = SupervisorPreparation {
            statuses: [super::super::render::LiveRenderPreparationStatus::Pending].into(),
            waited_for: Vec::new(),
        };

        let outcome = supervise_live_pump_preparation(
            &mut mailbox,
            &mut preparation,
            || Duration::ZERO,
            || None,
        )
        .expect("signal is returned to the production cancellation path");
        assert_eq!(
            outcome,
            LivePumpPreparationOutcome::End(LiveSupervisionEnd::Signal(LiveSignal::Terminate))
        );
        assert_eq!(preparation.waited_for, [LIVE_PREPARATION_MAILBOX_SLICE]);
    }

    #[test]
    fn sliced_render_preparation_accepts_readiness() {
        let mut mailbox = SupervisorMailbox::new([], [None]);
        let mut preparation = SupervisorPreparation {
            statuses: [super::super::render::LiveRenderPreparationStatus::Ready].into(),
            waited_for: Vec::new(),
        };

        assert_eq!(
            supervise_live_pump_preparation(
                &mut mailbox,
                &mut preparation,
                || Duration::ZERO,
                || None
            )
            .expect("readiness completes preparation"),
            LivePumpPreparationOutcome::Ready
        );
        assert_eq!(preparation.waited_for, [LIVE_PREPARATION_MAILBOX_SLICE]);
    }

    #[test]
    fn render_preparation_pending_for_thirty_seconds_times_out() {
        let mut mailbox = SupervisorMailbox::new([], [None, None]);
        let mut preparation = SupervisorPreparation {
            statuses: [super::super::render::LiveRenderPreparationStatus::Pending].into(),
            waited_for: Vec::new(),
        };
        let mut times = [
            Duration::ZERO,
            Duration::ZERO,
            LIVE_PUMP_PREPARATION_TIMEOUT,
        ]
        .into_iter();

        let error = supervise_live_pump_preparation(
            &mut mailbox,
            &mut preparation,
            || times.next().expect("preparation clock probe"),
            || None,
        )
        .expect_err("preparation has its own thirty-second deadline");
        assert!(error.to_string().contains("preparation"));
        assert_eq!(preparation.waited_for, [LIVE_PREPARATION_MAILBOX_SLICE]);
    }

    #[test]
    fn supervised_pump_polls_revocation_before_each_update() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                updated_reply(vec![submitted_event()]),
            ],
            [
                None,
                None,
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::SessionPause,
                )),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("revocation ends supervision"),
            LiveSupervisionEnd::Revocation(LiveRevocation::SessionPause)
        );
        assert_eq!(pump.commands, ["registration"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_pump_never_issues_a_second_update_after_revocation() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                updated_reply(vec![submitted_event()]),
            ],
            [
                None,
                None,
                None,
                None,
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::TargetHotplug,
                )),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();
        // The first poll is before the first update; the second observes the
        // revocation after its reply.
        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("revocation ends supervision"),
            LiveSupervisionEnd::Revocation(LiveRevocation::TargetHotplug)
        );
        assert_eq!(pump.commands, ["registration", "update"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_wedged_update_hits_the_two_second_deadline() {
        let mut mailbox = SupervisorMailbox::new([started_reply(), ready_reply(), None], []);
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("an update without a reply reaches the coordinator deadline");
        assert!(error.to_string().contains("no frame for 2s"));
        assert_eq!(pump.commands, ["registration", "update"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_registration_without_a_reply_hits_thirty_seconds() {
        let mut mailbox = SupervisorMailbox::new([started_reply(), None], []);
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("registration is coordinator-bounded");
        assert!(error.to_string().contains("registration"));
        assert_eq!(mailbox.waited_for.last(), Some(&REGISTRATION_TIMEOUT));
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_revocation_interrupts_registration_pending_backoff() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                pump_reply(PumpReply::Registration(Ok(LiveOutputRegistration::Pending))),
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::SessionPause,
                )),
            ],
            [None, None],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("revocation interrupts registration backoff"),
            LiveSupervisionEnd::Revocation(LiveRevocation::SessionPause)
        );
        assert_eq!(mailbox.waited_for.last(), Some(&Duration::from_millis(16)));
        assert_eq!(pump.commands, ["registration"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_repeated_registration_pending_reaches_thirty_seconds() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                pump_reply(PumpReply::Registration(Ok(LiveOutputRegistration::Pending))),
                None,
                pump_reply(PumpReply::Registration(Ok(LiveOutputRegistration::Pending))),
                None,
            ],
            [None, None, None, None],
        );
        mailbox.advances = [
            Duration::ZERO,
            Duration::ZERO,
            Duration::from_secs(15),
            Duration::ZERO,
            Duration::from_secs(15),
        ]
        .into();
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("pending registration cannot extend its fixed deadline");
        assert!(error.to_string().contains("registration"));
        assert_eq!(
            pump.commands,
            ["registration", "registration", "registration"]
        );
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_registration_ready_queued_at_deadline_wins() {
        let mut mailbox = SupervisorMailbox::new(
            [started_reply()],
            [
                None,
                ready_reply(),
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::SessionPause,
                )),
            ],
        );
        mailbox.advances = [REGISTRATION_TIMEOUT].into();
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("queued readiness wins the exact deadline"),
            LiveSupervisionEnd::Revocation(LiveRevocation::SessionPause)
        );
        assert_eq!(pump.commands, ["registration"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_slow_registration_starts_the_submit_clock_at_ready() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                pump_reply(PumpReply::Registration(Ok(LiveOutputRegistration::Pending))),
                None,
                ready_reply(),
                updated_reply(vec![submitted_event()]),
            ],
            [
                None,
                None,
                None,
                None,
                None,
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::SessionPause,
                )),
            ],
        );
        mailbox.advances = [
            Duration::ZERO,
            Duration::from_secs(3),
            Duration::from_millis(16),
            Duration::from_secs(3),
            Duration::from_millis(1_900),
        ]
        .into();
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("slow registration below 30s survives"),
            LiveSupervisionEnd::Revocation(LiveRevocation::SessionPause)
        );
        assert_eq!(pump.commands, ["registration", "registration", "update"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_startup_failure_latches_stop_before_cleanup() {
        let mut mailbox = SupervisorMailbox::new(
            [pump_reply(PumpReply::Started(Err(KmsLiveError::Setup(
                "injected pump startup failure".into(),
            ))))],
            [],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("startup failure reaches the bounded stop path");
        assert!(error.to_string().contains("injected pump startup failure"));
        assert!(pump.commands.is_empty());
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_late_submission_cannot_erase_the_deadline() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                updated_reply(vec![submitted_event()]),
            ],
            [],
        );
        mailbox.advances = [Duration::ZERO, Duration::ZERO, Duration::from_millis(2_200)].into();
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("a late reply remains terminal");
        assert!(error.to_string().contains("no frame for 2s"));
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_post_ready_worker_failure_is_terminal() {
        let mut mailbox = SupervisorMailbox::new(
            [
                started_reply(),
                ready_reply(),
                pump_reply(PumpReply::Updated(Err(KmsLiveError::Setup(
                    "injected-post-ready-worker-failure".into(),
                )))),
            ],
            [None, None, None, None],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let error = supervise_live_render(&mut mailbox, &mut pump, now)
            .expect_err("worker failure escapes production supervision");
        assert!(
            error
                .to_string()
                .contains("injected-post-ready-worker-failure")
        );
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn supervised_signals_share_the_bounded_stop_path() {
        for signal in [
            LiveSignal::Interrupt,
            LiveSignal::Terminate,
            LiveSignal::Hangup,
        ] {
            let mut mailbox = SupervisorMailbox::new(
                [started_reply(), ready_reply()],
                [Some(LiveCoordinatorEvent::Signal(signal))],
            );
            let mut pump = SupervisorPump::at_60_hz();
            let now = mailbox.now();
            assert_eq!(
                supervise_live_render(&mut mailbox, &mut pump, now)
                    .expect("signal ends supervision"),
                LiveSupervisionEnd::Signal(signal)
            );
            assert_eq!(pump.stops, 1);
            assert_eq!(
                KmsLiveError::Signal(signal).exit_code(),
                Some(signal.exit_code())
            );
        }
    }

    #[test]
    fn supervised_vt_switch_request_latches_stop_and_ends_gracefully() {
        let mut mailbox = SupervisorMailbox::new(
            [started_reply(), ready_reply()],
            [None, None, Some(LiveCoordinatorEvent::VtSwitchRequested(4))],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_live_render(&mut mailbox, &mut pump, now)
                .expect("an operator VT request ends supervision"),
            LiveSupervisionEnd::VtSwitchRequested {
                vt: 4,
                outstanding_command: None,
            }
        );
        assert_eq!(pump.commands, ["registration"]);
        assert_eq!(pump.stops, 1);
    }

    #[test]
    fn signal_handler_latches_before_yield_and_dominates_cleanup_exit_code() {
        assert_eq!(latched_signal_exit_code(), None);
        let latch = std::sync::atomic::AtomicI32::new(0);
        assert_eq!(
            live_signal_delivery_action(&latch, LiveSignal::Terminate),
            LiveSignalDeliveryAction::Latched
        );
        let latched = LiveSignal::from_number(latch.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(latched, Some(LiveSignal::Terminate));
        assert_eq!(
            live_signal_delivery_action(&latch, LiveSignal::Terminate),
            LiveSignalDeliveryAction::HardExit(143),
            "a repeated delivery is counted in the handler before iterator coalescing"
        );
        assert_eq!(
            live_signal_delivery_action(&latch, LiveSignal::Interrupt),
            LiveSignalDeliveryAction::HardExit(143),
            "a different second signal exits with the first signal's conventional code"
        );
        assert!(matches!(
            classify_pre_supervision_terminal(
                latched,
                Some(LiveCoordinatorEvent::Revocation(
                    LiveRevocation::SessionPause
                )),
                "during test preparation",
            ),
            Ok(Some(LiveSupervisionEnd::Signal(LiveSignal::Terminate)))
        ));

        let combined = combine_live_results(
            Err(KmsLiveError::Signal(LiveSignal::Terminate)),
            Err(KmsLiveError::PumpDetached("injected cleanup wedge".into())),
        )
        .expect_err("cleanup failure remains visible");
        assert!(matches!(combined, KmsLiveError::Setup(_)));
        assert_eq!(preferred_live_exit_code(&combined, latched), Some(143));
    }

    #[test]
    fn coordinator_sender_envelopes_revocations() {
        let (sender, receiver) = mpsc::channel();
        let coordinator = LiveCoordinatorSender(sender);
        let _clone = coordinator.sender();
        coordinator
            .send_revocation(LiveRevocation::ProtocolThreadStopped)
            .expect("coordinator mailbox remains open");
        assert!(matches!(
            receiver.recv().expect("enveloped event"),
            LiveCoordinatorEvent::Revocation(LiveRevocation::ProtocolThreadStopped)
        ));
        assert_eq!(
            KmsLiveError::PumpDetached("bounded detach".into()).reason_code(),
            "kms-live-pump-detached"
        );
        assert!(
            unexpected_pump_reply("test", PumpReply::Exited)
                .to_string()
                .contains("unexpected reply")
        );
    }

    #[test]
    fn typed_cancelled_cycles_do_not_trip_the_no_submit_watchdog() {
        let mut policy = SubmitWatchdog::new(Duration::ZERO);
        let events = [KmsRenderFrameEvent::PresentationCancelled {
            generation: 7,
            key: selected_output_for_test(41).key,
        }];
        observe_update_watchdog_evidence(&mut policy, &events, NO_SUBMIT_TIMEOUT)
            .expect("typed cancellation suspends the clock");
        assert!(!policy.no_submit_timed_out(NO_SUBMIT_TIMEOUT));
        assert!(
            !policy.no_submit_timed_out(
                NO_SUBMIT_TIMEOUT + NO_SUBMIT_TIMEOUT - Duration::from_millis(1)
            )
        );
        assert!(policy.no_submit_timed_out(NO_SUBMIT_TIMEOUT + NO_SUBMIT_TIMEOUT));
    }

    #[test]
    fn modeset_frame_within_its_budget_does_not_trip_the_no_submit_watchdog() {
        let bounded_gpu_and_modeset = cosmix_wgpu_dmabuf::RETIREMENT_WAIT_TIMEOUT
            + crate::backend::render::ATOMIC_MODESET_TIMEOUT;
        assert!(bounded_gpu_and_modeset < NO_SUBMIT_TIMEOUT);

        let mut policy = SubmitWatchdog::new(Duration::ZERO);
        let completed_at = bounded_gpu_and_modeset - Duration::from_millis(1);
        let events = [KmsRenderFrameEvent::FrameSubmitted {
            generation: 7,
            key: selected_output_for_test(41).key,
            frame_token: 1,
            timestamp: super::super::render::KmsPresentationTimestamp {
                seconds: 1,
                nanoseconds: 2,
            },
            security_epochs: Vec::new(),
        }];
        observe_update_watchdog_evidence(&mut policy, &events, completed_at)
            .expect("a modeset completing inside both bounded stages beats the watchdog");
        policy.observe_submitted(completed_at);
        assert!(!policy.no_submit_timed_out(completed_at));
    }

    #[test]
    fn empty_non_cancelled_updates_trip_the_no_submit_watchdog() {
        let mut policy = SubmitWatchdog::new(Duration::ZERO);
        let error = observe_update_watchdog_evidence(&mut policy, &[], NO_SUBMIT_TIMEOUT)
            .expect_err("an empty update is not cancellation evidence");
        assert_eq!(error.reason_code(), "kms-live-setup-failed");
        assert!(error.to_string().contains("submitted no frame"));
    }

    #[test]
    fn external_pause_cancels_active_presentation_generation_not_pause_generation() {
        use crate::backend::render::CancelScope;

        let active_generation = 41;
        let requested_pause_generation = active_generation + 1;
        let lifecycle = LiveCoordinatorLifecycle::active(active_generation, Duration::ZERO);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let presenter_observation = Arc::clone(&observed);
        let cancel = crate::backend::render::PresentationCancelHandle::fake(move |scope| {
            presenter_observation
                .lock()
                .expect("presenter observation lock")
                .push(scope);
        });
        let (pump, barrier) = crate::backend::render::LiveRenderPump::blocked_for_test_with_cancel(
            Duration::from_millis(20),
            cancel,
        );

        let cancelled =
            cancel_active_presentation_for_pause(&lifecycle, "external-pause", |generation| {
                pump.cancel_generation_presentations(generation)
            })
            .expect("active presentation is cancellable");

        assert_eq!(cancelled, active_generation);
        assert_eq!(
            *observed.lock().expect("presenter observation lock"),
            [CancelScope::Generation(active_generation)]
        );
        assert_ne!(
            *observed.lock().expect("presenter observation lock"),
            [CancelScope::Generation(requested_pause_generation)]
        );
        pump.begin_stop();
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    #[test]
    fn pause_cancel_fires_synchronously_for_active_generation() {
        use crate::backend::render::CancelScope;

        let order = Arc::new(Mutex::new(Vec::new()));
        let cancel_order = Arc::clone(&order);
        let cancel = crate::backend::render::PresentationCancelHandle::fake(move |scope| {
            if scope == CancelScope::Generation(41) {
                cancel_order
                    .lock()
                    .expect("pause-order observation lock")
                    .push("cancel");
            }
        });
        let (pump, barrier) = crate::backend::render::LiveRenderPump::blocked_for_test_with_cancel(
            Duration::from_millis(20),
            cancel,
        );
        let lifecycle = LiveCoordinatorLifecycle::active(41, Duration::ZERO);
        cancel_active_presentation_for_pause(&lifecycle, "external-pause", |generation| {
            pump.cancel_generation_presentations(generation)
        })
        .expect("external pause publishes cancellation");
        assert_eq!(
            *order.lock().expect("pause-order observation lock"),
            ["cancel"]
        );

        pump.begin_stop();
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    #[test]
    fn self_switch_cancel_fires_synchronously_for_active_generation() {
        use crate::backend::render::CancelScope;

        let order = Arc::new(Mutex::new(Vec::new()));
        let cancel_order = Arc::clone(&order);
        let cancel = crate::backend::render::PresentationCancelHandle::fake(move |scope| {
            if scope == CancelScope::Generation(73) {
                cancel_order
                    .lock()
                    .expect("self-switch order observation lock")
                    .push("cancel");
            }
        });
        let (pump, barrier) = crate::backend::render::LiveRenderPump::blocked_for_test_with_cancel(
            Duration::from_millis(20),
            cancel,
        );
        let lifecycle = LiveCoordinatorLifecycle::active(73, Duration::ZERO);
        cancel_active_presentation_for_pause(&lifecycle, "self-switch", |generation| {
            pump.cancel_generation_presentations(generation)
        })
        .expect("self-switch publishes cancellation");
        assert_eq!(
            *order.lock().expect("self-switch order observation lock"),
            ["cancel"]
        );

        pump.begin_stop();
        barrier.wait_for_stop();
        barrier.release_all_and_wait();
    }

    // Attended argv: opts into the typed-nonce interlock with `--kms-confirm`.
    // The bulk of the confirmation-mechanics tests below exercise that guard, so
    // this is the shared fixture for them. The production default is unattended;
    // `argv_unattended()` drops the flag for the tests that cover that path.
    fn argv() -> Vec<OsString> {
        [
            "kms-live",
            "--device",
            DEVICE,
            "--connector",
            CONNECTOR,
            "--kms-confirm",
        ]
        .map(OsString::from)
        .into()
    }

    fn argv_unattended() -> Vec<OsString> {
        ["kms-live", "--device", DEVICE, "--connector", CONNECTOR]
            .map(OsString::from)
            .into()
    }

    fn build() -> BuildProfile {
        BuildProfile {
            kms_live_feature: true,
            release: true,
        }
    }

    fn vt() -> VtState {
        VtState {
            observation_available: true,
            tty_is_character_device: true,
            tty_alias_rdev: libc::makedev(TTYAUX_MAJOR, TTY_ALIAS_MINOR),
            foreground_process_group: true,
            tty_major: LINUX_VT_MAJOR,
            tty_minor: 3,
            active_vt: Some(3),
        }
    }

    fn device() -> DeviceIdentity {
        DeviceIdentity {
            observation_available: true,
            observed_for: DEVICE.into(),
            canonical_path: Some(DEVICE.into()),
            node_is_character_device: true,
            node_is_primary_drm: true,
            node_rdev: libc::makedev(226, 0),
            udev_rdev: Some(libc::makedev(226, 0)),
            stable_device_path: Some(STABLE_DEVICE.into()),
            connectors: BTreeSet::from([CONNECTOR.into()]),
        }
    }

    fn refusal(
        argv: &[OsString],
        confirmation: &str,
        vt: VtState,
        build: BuildProfile,
        device: &DeviceIdentity,
    ) -> KmsLiveRefusal {
        decide(argv, confirmation, &TEST_NONCE, vt, build, device)
            .expect_err("sole falsifier must refuse")
    }

    #[test]
    fn complete_interlock_accepts_injected_fresh_facts() {
        let decision =
            decide(&argv(), CODE, &TEST_NONCE, vt(), build(), &device()).expect("valid interlock");
        assert_eq!(decision.request.device, Path::new(DEVICE));
        assert_eq!(decision.request.connector, CONNECTOR);
        assert_eq!(decision.request.scene_mode, LiveSceneMode::ClientContent);
        assert_eq!(decision.request.output_scale, OutputScale120::ONE);
        assert_eq!(decision.canonical_device, Path::new(DEVICE));
        assert_eq!(decision.vt, 3);
        assert_eq!(decision.stable_device_path, Path::new(STABLE_DEVICE));
    }

    #[test]
    fn scale_cli_is_exact_defaults_to_one_and_accepts_250_percent() {
        assert_eq!(
            parse_request(&argv())
                .expect("default request")
                .output_scale,
            OutputScale120::ONE
        );
        for value in ["2.5", "2.500", "+2.5"] {
            let mut scaled = argv();
            scaled.extend(["--scale", value].map(OsString::from));
            assert_eq!(
                parse_request(&scaled)
                    .unwrap_or_else(|error| panic!("{value} should parse exactly: {error}"))
                    .output_scale
                    .get(),
                300
            );
        }
        let mut fine = argv();
        fine.extend(["--scale", "1.025"].map(OsString::from));
        assert_eq!(
            parse_request(&fine)
                .expect("three 120ths is exactly 0.025")
                .output_scale
                .get(),
            123
        );
    }

    #[test]
    fn scale_cli_rejects_missing_duplicate_non_finite_non_positive_and_non_120th_values() {
        let refusal_for = |value: &str| {
            let mut invalid = argv();
            invalid.extend(["--scale", value].map(OsString::from));
            parse_request(&invalid).expect_err("invalid scale must refuse")
        };
        for value in ["NaN", "inf", "+inf", "-inf", "1e0", ".5"] {
            assert_eq!(refusal_for(value), KmsLiveRefusal::InvalidScale, "{value}");
        }
        for value in ["0", "0.0", "-1", "-0.5"] {
            assert_eq!(
                refusal_for(value),
                KmsLiveRefusal::NonPositiveScale,
                "{value}"
            );
        }
        for value in ["2.51", "1.008333"] {
            assert_eq!(refusal_for(value), KmsLiveRefusal::Non120thScale, "{value}");
        }

        let mut missing = argv();
        missing.push("--scale".into());
        assert_eq!(
            parse_request(&missing).expect_err("missing scale value"),
            KmsLiveRefusal::MissingScale
        );
        let mut duplicate = argv();
        duplicate.extend(["--scale", "2.5", "--scale", "1.0"].map(OsString::from));
        assert_eq!(
            parse_request(&duplicate).expect_err("duplicate scale"),
            KmsLiveRefusal::DuplicateScale
        );
    }

    #[test]
    fn typed_confirmation_intent_names_the_exact_scale() {
        let mut scaled = argv();
        scaled.extend(["--scale", "2.5"].map(OsString::from));
        let request = parse_request(&scaled).expect("scaled request");
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let grant =
            authorise_observed(request, build(), harmless_fd(), platform, &mut confirmation)
                .expect("scaled request passes injected interlock");

        assert_eq!(grant.output_scale.get(), 300);
        assert_eq!(
            confirmation.displayed_intent.as_deref(),
            Some(
                "About to take DRM master of /dev/dri/card0 (eDP-1) on tty3 with requested scale 2.5; the physical mode will be selected after confirmation."
            )
        );
    }

    #[test]
    fn client_content_is_the_default_live_scene_mode() {
        let decision = decide(&argv(), CODE, &TEST_NONCE, vt(), build(), &device())
            .expect("valid default client-content interlock");
        assert_eq!(decision.request.scene_mode, LiveSceneMode::ClientContent);
        assert!(decision.request.decoration.enabled);
        assert_eq!(decision.request.decoration.theme.style, ChromeStyle::Mac);
    }

    #[test]
    fn first_light_is_an_opt_out_live_scene_mode_and_rejects_a_duplicate() {
        let mut requested = argv();
        requested.push("--first-light".into());
        let decision = decide(&requested, CODE, &TEST_NONCE, vt(), build(), &device())
            .expect("valid first-light interlock");
        assert_eq!(decision.request.scene_mode, LiveSceneMode::FirstLight);
        assert!(!decision.request.decoration.enabled);

        requested.push("--first-light".into());
        assert_eq!(
            refusal(&requested, CODE, vt(), build(), &device()),
            KmsLiveRefusal::DuplicateFirstLight
        );
    }

    #[test]
    fn kms_live_ssd_cli_accepts_default_explicit_on_off_and_chrome() {
        let implicit = parse_request(&argv()).expect("SSD defaults on");
        assert!(implicit.decoration.enabled);

        let mut requested = argv();
        requested.push("--ssd".into());
        let request = parse_request(&requested).expect("SSD with default chrome");
        assert!(request.decoration.enabled);
        assert_eq!(request.decoration.theme.style, ChromeStyle::Mac);

        for (name, style) in [
            ("mac", ChromeStyle::Mac),
            ("win11", ChromeStyle::Win11),
            ("cosmix", ChromeStyle::Cosmix),
        ] {
            let mut requested = argv();
            requested.extend(["--chrome", name].map(OsString::from));
            let request = parse_request(&requested).expect("built-in live chrome style");
            assert!(request.decoration.enabled);
            assert_eq!(request.decoration.theme.style, style);
        }

        let mut disabled = argv();
        disabled.push("--no-ssd".into());
        assert!(
            !parse_request(&disabled)
                .expect("explicit SSD opt-out")
                .decoration
                .enabled
        );

        let mut first_light_disabled = argv();
        first_light_disabled.extend(["--first-light", "--no-ssd"].map(OsString::from));
        assert!(
            !parse_request(&first_light_disabled)
                .expect("first-light accepts the explicit opt-out")
                .decoration
                .enabled
        );
    }

    #[test]
    fn kms_live_ssd_cli_rejects_invalid_combinations() {
        let mut invalid_chrome = argv();
        invalid_chrome.extend(["--ssd", "--chrome", "unknown"].map(OsString::from));
        assert_eq!(
            parse_request(&invalid_chrome).expect_err("unknown chrome is refused"),
            KmsLiveRefusal::InvalidChrome
        );

        let mut first_light = argv();
        first_light.extend(["--first-light", "--ssd"].map(OsString::from));
        assert_eq!(
            parse_request(&first_light).expect_err("first-light cannot own SSD"),
            KmsLiveRefusal::DecorationFirstLightConflict
        );

        let mut first_light_chrome = argv();
        first_light_chrome.extend(["--first-light", "--chrome", "mac"].map(OsString::from));
        assert_eq!(
            parse_request(&first_light_chrome).expect_err("first-light cannot select chrome"),
            KmsLiveRefusal::DecorationFirstLightConflict
        );

        let mut duplicate_ssd = argv();
        duplicate_ssd.extend(["--ssd", "--ssd"].map(OsString::from));
        assert_eq!(
            parse_request(&duplicate_ssd).expect_err("duplicate SSD switch"),
            KmsLiveRefusal::DuplicateSsd
        );

        let mut duplicate_no_ssd = argv();
        duplicate_no_ssd.extend(["--no-ssd", "--no-ssd"].map(OsString::from));
        assert_eq!(
            parse_request(&duplicate_no_ssd).expect_err("duplicate SSD opt-out"),
            KmsLiveRefusal::DuplicateNoSsd
        );

        for switches in [["--ssd", "--no-ssd"], ["--no-ssd", "--ssd"]] {
            let mut conflict = argv();
            conflict.extend(switches.map(OsString::from));
            assert_eq!(
                parse_request(&conflict).expect_err("opposed SSD switches conflict"),
                KmsLiveRefusal::SsdNoSsdConflict
            );
        }

        for switches in [
            ["--no-ssd", "--chrome", "mac"],
            ["--chrome", "mac", "--no-ssd"],
        ] {
            let mut conflict = argv();
            conflict.extend(switches.map(OsString::from));
            assert_eq!(
                parse_request(&conflict).expect_err("disabled SSD cannot select chrome"),
                KmsLiveRefusal::NoSsdChromeConflict
            );
        }

        let mut duplicate_chrome = argv();
        duplicate_chrome
            .extend(["--ssd", "--chrome", "mac", "--chrome", "win11"].map(OsString::from));
        assert_eq!(
            parse_request(&duplicate_chrome).expect_err("duplicate chrome switch"),
            KmsLiveRefusal::DuplicateChrome
        );
    }

    #[test]
    fn kms_confirm_flag_opts_into_the_typed_nonce_gate() {
        // The interlock is opt-in: the default (unattended) request carries
        // `confirm = false` so a live takeover proceeds without a human at the
        // glass; `--kms-confirm` is the only thing that turns the typed-nonce
        // challenge back on.
        assert!(
            !parse_request(&argv_unattended())
                .expect("unattended request parses")
                .confirm
        );
        assert!(
            parse_request(&argv())
                .expect("attended request parses")
                .confirm
        );
    }

    #[test]
    fn presentation_cli_defaults_to_atomic_and_refuses_retired_direct_display() {
        assert_eq!(
            parse_request(&argv())
                .expect("default presentation request parses")
                .presentation_backend,
            PresentationBackend::Atomic
        );

        let mut atomic = argv();
        atomic.extend(["--presentation", "atomic"].map(OsString::from));
        assert_eq!(
            parse_request(&atomic)
                .expect("explicit atomic request parses")
                .presentation_backend,
            PresentationBackend::Atomic
        );

        let mut retired = argv();
        retired.extend(["--presentation", "direct-display"].map(OsString::from));
        let error = parse_request(&retired).expect_err("retired backend refuses by name");
        assert_eq!(error, KmsLiveRefusal::DirectDisplayRetired);
        assert_eq!(error.reason_code(), "kms-live-direct-display-retired");
    }

    #[test]
    fn atomic_backend_is_the_only_admitted_runtime_backend() {
        assert!(validate_presentation_backend(PresentationBackend::Atomic).is_ok());
    }

    #[test]
    fn presentation_cli_rejects_missing_duplicate_and_invalid_values() {
        let mut missing = argv();
        missing.push("--presentation".into());
        assert_eq!(
            parse_request(&missing).expect_err("presentation value is required"),
            KmsLiveRefusal::MissingPresentation
        );

        let mut duplicate = argv();
        duplicate
            .extend(["--presentation", "atomic", "--presentation", "atomic"].map(OsString::from));
        assert_eq!(
            parse_request(&duplicate).expect_err("presentation is single-valued"),
            KmsLiveRefusal::DuplicatePresentation
        );

        let mut invalid = argv();
        invalid.extend(["--presentation", "auto"].map(OsString::from));
        assert_eq!(
            parse_request(&invalid).expect_err("automatic fallback is not a backend"),
            KmsLiveRefusal::InvalidPresentation
        );
    }

    #[test]
    fn duplicate_kms_confirm_is_refused() {
        // `argv()` already supplies one `--kms-confirm`; a second is the
        // duplicate. Refusing it keeps the flag single-valued like every other
        // switch on this surface.
        let mut duplicate = argv();
        duplicate.push("--kms-confirm".into());
        assert_eq!(
            parse_request(&duplicate).expect_err("duplicate --kms-confirm must refuse"),
            KmsLiveRefusal::DuplicateKmsConfirm
        );
    }

    #[test]
    fn removed_client_content_flag_is_an_unknown_argument() {
        let mut requested = argv();
        requested.push("--client-content".into());
        assert_eq!(
            refusal(&requested, CODE, vt(), build(), &device()),
            KmsLiveRefusal::UnknownArgument
        );
    }

    #[test]
    fn subcommand_position_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.swap(0, 1);
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::SubcommandNotFirst
        );
    }

    #[test]
    fn unknown_argument_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.push("--surprise".into());
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::UnknownArgument
        );
    }

    #[test]
    fn argv_confirmation_is_rejected_as_an_unknown_argument() {
        let mut invalid = argv();
        invalid.extend(["--confirm-drm-takeover", CODE].map(OsString::from));
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::UnknownArgument
        );
    }

    #[test]
    fn required_device_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.drain(1..=2);
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::MissingDevice
        );
    }

    #[test]
    fn unique_device_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.extend(["--device", DEVICE].map(OsString::from));
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::DuplicateDevice
        );
    }

    #[test]
    fn absolute_device_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid[2] = "card0".into();
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::InvalidDevice
        );
    }

    #[test]
    fn required_connector_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.drain(3..=4);
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::MissingConnector
        );
    }

    #[test]
    fn unique_connector_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid.extend(["--connector", CONNECTOR].map(OsString::from));
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::DuplicateConnector
        );
    }

    #[test]
    fn connector_syntax_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid[4] = "../eDP-1".into();
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::InvalidConnector
        );
    }

    #[test]
    fn empty_connector_has_a_sole_falsifier() {
        let mut invalid = argv();
        invalid[4] = "".into();
        assert_eq!(
            refusal(&invalid, CODE, vt(), build(), &device()),
            KmsLiveRefusal::InvalidConnector
        );
    }

    #[test]
    fn atomic_default_without_kms_live_feature_has_a_named_refusal() {
        assert_eq!(
            parse_request(&argv())
                .expect("default presentation request parses")
                .presentation_backend,
            PresentationBackend::Atomic
        );
        let mut invalid = build();
        invalid.kms_live_feature = false;
        let error = refusal(&argv(), CODE, vt(), invalid, &device());
        assert_eq!(error, KmsLiveRefusal::FeatureDisabled);
        assert_eq!(error.reason_code(), "kms-live-feature-disabled");
    }

    #[test]
    fn cargo_release_profile_has_a_sole_falsifier() {
        let mut invalid = build();
        invalid.release = false;
        assert_eq!(
            refusal(&argv(), CODE, vt(), invalid, &device()),
            KmsLiveRefusal::ReleaseBuildRequired
        );
    }

    #[test]
    fn vt_observation_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.observation_available = false;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::VtObservationUnavailable
        );
    }

    #[test]
    fn vt_getstate_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.active_vt = None;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::VtObservationUnavailable
        );
    }

    #[test]
    fn tty_character_device_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.tty_is_character_device = false;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::TtyNotCharacterDevice
        );
    }

    #[test]
    fn tty_alias_identity_has_a_sole_falsifier() {
        for rdev in [
            libc::makedev(LINUX_VT_MAJOR, 1),
            libc::makedev(TTYAUX_MAJOR, 1),
        ] {
            let mut invalid = vt();
            invalid.tty_alias_rdev = rdev;
            assert_eq!(
                refusal(&argv(), CODE, invalid, build(), &device()),
                KmsLiveRefusal::TtyNotKernelAlias
            );
        }
    }

    #[test]
    fn foreground_process_group_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.foreground_process_group = false;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::TtyNotForegroundProcessGroup
        );
    }

    #[test]
    fn real_linux_vt_major_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.tty_major = 136;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::TtyNotVirtualTerminal
        );
    }

    #[test]
    fn zero_linux_vt_minor_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.tty_minor = 0;
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::TtyNotVirtualTerminal
        );
    }

    #[test]
    fn out_of_range_linux_vt_minor_has_a_sole_falsifier() {
        let mut invalid = vt();
        invalid.tty_minor = MAX_LINUX_VT + 1;
        invalid.active_vt = Some((MAX_LINUX_VT + 1) as u16);
        assert_eq!(
            refusal(&argv(), CODE, invalid, build(), &device()),
            KmsLiveRefusal::TtyNotVirtualTerminal
        );
    }

    #[test]
    fn device_observation_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.observation_available = false;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceObservationUnavailable
        );
    }

    #[test]
    fn canonical_device_observation_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.canonical_path = None;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceObservationUnavailable
        );
    }

    #[test]
    fn device_observation_target_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.observed_for = "/dev/dri/card1".into();
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceObservationTargetMismatch
        );
    }

    #[test]
    fn drm_character_device_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.node_is_character_device = false;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceNotCharacterDevice
        );
    }

    #[test]
    fn primary_drm_node_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.node_is_primary_drm = false;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceNotPrimaryNode
        );
    }

    #[test]
    fn udev_device_identity_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.udev_rdev = None;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceMissingUdevIdentity
        );
    }

    #[test]
    fn exact_device_rdev_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.udev_rdev = Some(libc::makedev(226, 1));
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceRdevMismatch
        );
    }

    #[test]
    fn stable_device_identity_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.stable_device_path = None;
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::DeviceStableIdentityUnavailable
        );
    }

    #[test]
    fn requested_connector_presence_has_a_sole_falsifier() {
        let mut invalid = device();
        invalid.connectors.clear();
        assert_eq!(
            refusal(&argv(), CODE, vt(), build(), &invalid),
            KmsLiveRefusal::ConnectorNotPresent
        );
    }

    #[test]
    fn wrong_confirmation_code_has_a_sole_falsifier() {
        assert_eq!(
            refusal(&argv(), "5a5a5a5b", vt(), build(), &device()),
            KmsLiveRefusal::ConfirmationMismatch
        );
    }

    struct FakePlatform {
        states: RefCell<VecDeque<VtState>>,
        devices: RefCell<VecDeque<DeviceIdentity>>,
        legacy_tiocsti: Cell<Result<bool, KmsLiveRefusal>>,
        nonce: Cell<Result<[u8; CONFIRMATION_NONCE_BYTES], KmsLiveRefusal>>,
        incarnation_hold_error: Cell<Option<KmsLiveRefusal>>,
        incarnation_validation: Cell<Result<(), KmsLiveRefusal>>,
        drm_open_error: Cell<Option<KmsLiveRefusal>>,
        opened_identity: RefCell<Result<OpenDrmIdentity, KmsLiveRefusal>>,
        connector: Cell<Result<Option<ConnectorBinding>, KmsLiveRefusal>>,
        drm_open_count: Cell<usize>,
        incarnation_validation_count: Cell<usize>,
        connector_scan_count: Cell<usize>,
        boundary_events: RefCell<Vec<&'static str>>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RecordedTtyKernelCall {
        Flush {
            fd: libc::c_int,
            selector: libc::c_int,
        },
        ForegroundProcessGroup {
            fd: libc::c_int,
        },
        CallerProcessGroup,
        TtyDevice {
            fd: libc::c_int,
            request: libc::c_ulong,
        },
        VtState {
            fd: libc::c_int,
            request: libc::c_ulong,
        },
    }

    struct RecordingTtyKernel {
        calls: RefCell<Vec<RecordedTtyKernelCall>>,
        flush_result: Cell<libc::c_int>,
        /// What `getpgrp` answers; 27 matches `tcgetpgrp`'s fixed answer, so a
        /// test sets a different value to be a non-foreground caller.
        caller_pgrp: Cell<libc::pid_t>,
    }

    impl Default for RecordingTtyKernel {
        fn default() -> Self {
            Self {
                calls: RefCell::default(),
                flush_result: Cell::new(0),
                caller_pgrp: Cell::new(27),
            }
        }
    }

    impl TtyKernelCalls for RecordingTtyKernel {
        fn tcflush(&self, fd: libc::c_int, selector: libc::c_int) -> libc::c_int {
            self.calls
                .borrow_mut()
                .push(RecordedTtyKernelCall::Flush { fd, selector });
            self.flush_result.get()
        }

        fn tcgetpgrp(&self, fd: libc::c_int) -> libc::pid_t {
            self.calls
                .borrow_mut()
                .push(RecordedTtyKernelCall::ForegroundProcessGroup { fd });
            27
        }

        fn getpgrp(&self) -> libc::pid_t {
            self.calls
                .borrow_mut()
                .push(RecordedTtyKernelCall::CallerProcessGroup);
            self.caller_pgrp.get()
        }

        fn tiocgdev(
            &self,
            fd: libc::c_int,
            request: libc::c_ulong,
            output: &mut u32,
        ) -> libc::c_int {
            self.calls
                .borrow_mut()
                .push(RecordedTtyKernelCall::TtyDevice { fd, request });
            *output = libc::makedev(LINUX_VT_MAJOR, 3) as u32;
            0
        }

        fn vt_getstate(
            &self,
            fd: libc::c_int,
            request: libc::c_ulong,
            output: &mut LinuxVtStat,
        ) -> libc::c_int {
            self.calls
                .borrow_mut()
                .push(RecordedTtyKernelCall::VtState { fd, request });
            output.active = 3;
            0
        }
    }

    impl FakePlatform {
        fn new(states: impl IntoIterator<Item = VtState>) -> Self {
            Self {
                states: RefCell::new(states.into_iter().collect()),
                devices: RefCell::new([device(), device()].into()),
                legacy_tiocsti: Cell::new(Ok(false)),
                nonce: Cell::new(Ok(TEST_NONCE)),
                incarnation_hold_error: Cell::new(None),
                incarnation_validation: Cell::new(Ok(())),
                drm_open_error: Cell::new(None),
                opened_identity: RefCell::new(Ok(opened_identity(STABLE_DEVICE))),
                connector: Cell::new(Ok(Some(ConnectorBinding { connector_id: 31 }))),
                drm_open_count: Cell::new(0),
                incarnation_validation_count: Cell::new(0),
                connector_scan_count: Cell::new(0),
                boundary_events: RefCell::new(Vec::new()),
            }
        }
    }

    impl GrantPlatform for FakePlatform {
        fn observe_vt(&self, _tty: BorrowedFd<'_>) -> VtState {
            self.boundary_events.borrow_mut().push("vt");
            self.states
                .borrow_mut()
                .pop_front()
                .expect("fake VT observation")
        }

        fn observe_device(&self, _request: &KmsLiveRequest) -> DeviceIdentity {
            self.devices
                .borrow_mut()
                .pop_front()
                .expect("fake device observation")
        }

        fn legacy_tiocsti_enabled(&self) -> Result<bool, KmsLiveRefusal> {
            self.legacy_tiocsti.get()
        }

        fn fill_confirmation_nonce(&self, nonce: &mut [u8]) -> Result<(), KmsLiveRefusal> {
            let generated = self.nonce.get()?;
            nonce.copy_from_slice(&generated);
            Ok(())
        }

        fn hold_device_incarnation(
            &self,
            device: &DeviceIdentity,
        ) -> Result<DeviceIncarnationWitness, KmsLiveRefusal> {
            if let Some(error) = self.incarnation_hold_error.get() {
                return Err(error);
            }
            Ok(DeviceIncarnationWitness {
                dev_attribute: harmless_fd(),
                card_inode: 41,
                expected_rdev: device.node_rdev,
            })
        }

        fn validate_device_incarnation(
            &self,
            witness: &DeviceIncarnationWitness,
            _opened: &OpenDrmIdentity,
        ) -> Result<(), KmsLiveRefusal> {
            let _held_exact_incarnation = (
                witness.dev_attribute.as_fd(),
                witness.card_inode,
                witness.expected_rdev,
            );
            self.boundary_events.borrow_mut().push("incarnation");
            self.incarnation_validation_count
                .set(self.incarnation_validation_count.get().saturating_add(1));
            self.incarnation_validation.get()
        }

        fn observe_open_drm(&self, _fd: BorrowedFd<'_>) -> Result<OpenDrmIdentity, KmsLiveRefusal> {
            self.boundary_events.borrow_mut().push("observe-open");
            self.opened_identity.borrow().clone()
        }

        fn scan_connector(
            &self,
            _fd: BorrowedFd<'_>,
            _opened: &OpenDrmIdentity,
            _connector: &str,
        ) -> Result<Option<ConnectorBinding>, KmsLiveRefusal> {
            self.boundary_events.borrow_mut().push("connector");
            self.connector_scan_count
                .set(self.connector_scan_count.get().saturating_add(1));
            self.connector.get()
        }
    }

    struct FakeConfirmation {
        queued_before_prompt: Option<String>,
        flush_result: libc::c_int,
        after_prompt: Option<Result<String, KmsLiveRefusal>>,
        prompt_displayed: bool,
        displayed_intent: Option<String>,
        displayed_code: Option<String>,
        events: Vec<&'static str>,
    }

    impl FakeConfirmation {
        fn typed(code: &str) -> Self {
            Self {
                queued_before_prompt: None,
                flush_result: 0,
                after_prompt: Some(Ok(code.into())),
                prompt_displayed: false,
                displayed_intent: None,
                displayed_code: None,
                events: Vec::new(),
            }
        }
    }

    impl ConfirmationIo for FakeConfirmation {
        fn flush_input(&mut self, _tty: BorrowedFd<'_>) -> Result<(), KmsLiveRefusal> {
            self.events.push("flush");
            require_input_flush(self.flush_result)?;
            self.queued_before_prompt = None;
            Ok(())
        }

        fn display_prompt(
            &mut self,
            _tty: BorrowedFd<'_>,
            intent: &str,
            expected_code: &str,
        ) -> Result<(), KmsLiveRefusal> {
            self.events.push("prompt");
            self.prompt_displayed = true;
            self.displayed_intent = Some(intent.to_owned());
            self.displayed_code = Some(expected_code.to_owned());
            Ok(())
        }

        fn read_line(&mut self, _tty: BorrowedFd<'_>) -> Result<String, KmsLiveRefusal> {
            self.events.push("read");
            self.after_prompt
                .take()
                .or_else(|| self.queued_before_prompt.take().map(Ok))
                .unwrap_or(Err(KmsLiveRefusal::ConfirmationReadFailed))
        }
    }

    fn opened_identity(stable_device_path: &str) -> OpenDrmIdentity {
        OpenDrmIdentity {
            rdev: libc::makedev(226, 0),
            stable_device_path: stable_device_path.into(),
            sysfs_card_path: SYSFS_CARD.into(),
        }
    }

    fn harmless_fd() -> OwnedFd {
        UnixStream::pair().expect("socket pair").0.into()
    }

    fn pipe_fd() -> OwnedFd {
        smithay::reexports::rustix::pipe::pipe()
            .expect("offline ownership pipe")
            .0
    }

    #[test]
    fn preferred_rejection_uses_the_admitted_fallback_for_bootstrap_geometry() {
        use super::super::kms::{
            AtomicOutputSelection, ConnectorDescription, ConnectorMode, KmsTopologySnapshot,
            OutputKey, OutputScale120, PreselectedAtomicOutput,
        };

        let key = OutputKey {
            device: 226,
            connector_name: "Offline-1".into(),
        };
        let mode = |width, height, preferred| {
            let width_timing = u16::try_from(width).expect("test width fits DRM timing");
            let height_timing = u16::try_from(height).expect("test height fits DRM timing");
            ConnectorMode {
                width,
                height,
                refresh_millihz: 60_000,
                preferred,
                clock_khz: 148_500,
                hsync: (width_timing, width_timing + 8, width_timing + 80),
                vsync: (height_timing, height_timing + 4, height_timing + 40),
                hskew: 0,
                vscan: 0,
                flags: 0,
            }
        };
        let preferred = mode(2560, 1440, true);
        let fallback = mode(1920, 1080, false);
        let topology = KmsTopologySnapshot {
            connectors: vec![ConnectorDescription {
                key: key.clone(),
                connector_id: 41,
                modes: vec![preferred, fallback],
            }],
            selections: vec![
                PreselectedAtomicOutput {
                    key: key.clone(),
                    connector_mode: preferred,
                    selection: Err("preferred mode rejected by atomic admission".into()),
                },
                PreselectedAtomicOutput {
                    key,
                    connector_mode: fallback,
                    selection: Ok(AtomicOutputSelection {
                        connector_id: 41,
                        crtc_id: 151,
                        primary_plane_id: 31,
                        mode: fallback,
                        format: u32::from_le_bytes(*b"XR24"),
                        modifier: 0,
                    }),
                },
            ],
            output_scale: OutputScale120::ONE,
        };

        assert_eq!(
            admitted_bootstrap_extent(&topology).expect("fallback mode is admitted"),
            (1920, 1080),
            "popup constraints must use the same extent as the admitted output"
        );

        let key = OutputKey {
            device: 226,
            connector_name: "Offline-4K".into(),
        };
        let physical = mode(3840, 2160, true);
        let fractional = KmsTopologySnapshot {
            connectors: vec![ConnectorDescription {
                key: key.clone(),
                connector_id: 42,
                modes: vec![physical],
            }],
            selections: vec![PreselectedAtomicOutput {
                key,
                connector_mode: physical,
                selection: Ok(AtomicOutputSelection {
                    connector_id: 42,
                    crtc_id: 152,
                    primary_plane_id: 32,
                    mode: physical,
                    format: u32::from_le_bytes(*b"XR24"),
                    modifier: 0,
                }),
            }],
            output_scale: OutputScale120::new(300).expect("250 percent"),
        };
        assert_eq!(
            admitted_bootstrap_extent(&fractional).expect("fractional 4K mode is admitted"),
            (1536, 864),
            "pre-first-render popup constraints bootstrap in logical coordinates"
        );
    }

    #[test]
    fn cyclic_session_authority_advances_without_reusing_a_generation() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        for generation in 2..=21 {
            assert!(latch_live_revocation(
                &mut authority,
                LiveRevocation::SessionPause
            ));
            assert_eq!(
                authority.begin_resume().expect("resume generation"),
                generation
            );
            assert_eq!(authority, LiveSessionAuthority::Preparing { generation });
            authority = LiveSessionAuthority::Active { generation };
        }
    }

    #[test]
    fn pause_after_the_vt_request_is_submitted_confirms_the_self_switch() {
        assert_eq!(SELF_SWITCH_PAUSE_TIMEOUT, Duration::from_secs(1));
        assert_eq!(LIVE_INPUT_LIFECYCLE_TIMEOUT, Duration::from_secs(5));
        assert_eq!(LIVE_RESUME_TIMEOUT, Duration::from_secs(30));
        assert_eq!(EXTERNAL_PAUSE_ACK_TIMEOUT, Duration::from_secs(45));
        assert_eq!(EXTERNAL_PAUSED_TIMEOUT, Duration::from_secs(5));
        assert_eq!(
            LIVE_RESUME_BACKOFFS,
            [Duration::from_millis(50), Duration::from_millis(100)]
        );
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        authority
            .begin_self_switch(2)
            .expect("first-cause self-switch starts generation two");
        assert_eq!(
            authority,
            LiveSessionAuthority::SelfSwitching { generation: 2 }
        );
        authority
            .submit_self_switch()
            .expect("change_vt submission establishes self-switch causality");
        assert_eq!(authority.confirm_self_pause(), Some(2));
        assert_eq!(
            authority.confirm_self_pause(),
            None,
            "a duplicate deferred-notifier pause does not tear down twice"
        );
        assert_eq!(authority.activated_self_pause(), Some(2));
        assert!(matches!(
            LiveCoordinatorEvent::SessionPauseConfirmed { generation: 2 },
            LiveCoordinatorEvent::SessionPauseConfirmed { generation: 2 }
        ));
        assert!(matches!(
            LiveCoordinatorEvent::SessionActivate { generation: 2 },
            LiveCoordinatorEvent::SessionActivate { generation: 2 }
        ));
        assert_eq!(authority.begin_resume().expect("resume begins"), 3);
        authority = LiveSessionAuthority::Active { generation: 3 };
        authority
            .return_to_self_paused(5)
            .expect("failed attempt returns at the compensating suspend generation");
        assert_eq!(
            authority,
            LiveSessionAuthority::SelfPaused {
                generation: 5,
                pause_confirmed: true,
            }
        );

        let mut successful = LiveSessionAuthority::Active { generation: 3 };
        successful
            .finish_resume(4)
            .expect("the one resumed output advances authority exactly once");
        assert_eq!(successful, LiveSessionAuthority::Active { generation: 4 });
    }

    #[test]
    fn pause_before_the_vt_request_is_submitted_establishes_external_causality() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        authority
            .begin_self_switch(2)
            .expect("self-switch preparation begins");
        assert_eq!(authority.confirm_self_pause(), None);
        assert_eq!(
            authority.request_pause().expect("pause is classified"),
            LivePauseRequestDisposition::External { generation: 2 }
        );
        assert_eq!(
            authority,
            LiveSessionAuthority::ExternalPausing {
                generation: 2,
                activate_pending: false,
            }
        );
        assert_eq!(
            authority.complete_pause(true),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::External,
                resumable: true,
                activate_pending: false,
            })
        );
        assert!(self_switch_was_not_submitted(&VtSwitchAsk::Refused(
            SELF_SWITCH_NOT_PREPARED.into()
        )));
    }

    #[test]
    fn racing_self_switch_and_external_pause_keep_the_first_cause_and_generation() {
        let mut external_first = LiveSessionAuthority::Active { generation: 1 };
        external_first
            .begin_self_switch(2)
            .expect("self switch is preparing but not submitted");
        assert_eq!(
            external_first.request_pause().expect("external pause wins"),
            LivePauseRequestDisposition::External { generation: 2 }
        );
        assert!(matches!(
            external_first.begin_self_switch(2),
            Err(KmsLiveError::AuthorityLost(LiveRevocation::SessionPause))
        ));

        let mut self_first = LiveSessionAuthority::Active { generation: 1 };
        self_first
            .begin_self_switch(2)
            .expect("self switch preparation begins");
        self_first
            .submit_self_switch()
            .expect("change_vt submission wins causality");
        assert_eq!(
            self_first.request_pause().expect("raw pause is classified"),
            LivePauseRequestDisposition::SelfSwitch { generation: 2 }
        );
        assert_eq!(
            self_first.complete_pause(true),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::SelfSwitch,
                resumable: true,
                activate_pending: false,
            })
        );
        assert_eq!(
            self_first.request_pause().expect("duplicate is classified"),
            LivePauseRequestDisposition::Duplicate,
            "the losing external event cannot create another generation"
        );
    }

    #[test]
    fn self_switch_resumes_after_a_pause_longer_than_the_disable_ack_window() {
        let acknowledgement_window = Duration::from_millis(2);
        let (_unanswered, answer) = mpsc::sync_channel::<()>(1);
        let acknowledgement = match answer.recv_timeout(acknowledgement_window) {
            Err(mpsc::RecvTimeoutError::Timeout) => DeferredDisableAcknowledgementOutcome::TimedOut,
            outcome => panic!("injected acknowledgement window did not expire: {outcome:?}"),
        };
        let outcome = DeferredDisableOutcome {
            acknowledgement,
            disable_succeeded: true,
        };

        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        authority
            .begin_self_switch(2)
            .expect("self-switch preparation begins");
        authority
            .submit_self_switch()
            .expect("change_vt submission establishes self-switch causality");
        assert_eq!(
            authority.request_pause().expect("raw pause is classified"),
            LivePauseRequestDisposition::SelfSwitch { generation: 2 }
        );
        assert!(deferred_pause_is_resumable(authority, outcome));
        assert_eq!(
            authority.complete_pause(deferred_pause_is_resumable(authority, outcome)),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::SelfSwitch,
                resumable: true,
                activate_pending: false,
            })
        );

        std::thread::sleep(acknowledgement_window.saturating_mul(2));
        assert_eq!(
            authority.activate(),
            Some(2),
            "Enable remains deliverable after an arbitrarily long self pause"
        );
        assert_eq!(authority.begin_resume().expect("resume begins"), 3);

        let mut external = LiveSessionAuthority::Active { generation: 1 };
        assert_eq!(
            external.request_pause().expect("external pause begins"),
            LivePauseRequestDisposition::External { generation: 2 }
        );
        assert!(
            !deferred_pause_is_resumable(external, outcome),
            "an unacknowledged external pause still fails closed"
        );
    }

    #[test]
    fn activate_before_paused_and_duplicate_events_are_generation_stable() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        assert_eq!(
            authority.request_pause().expect("external pause starts"),
            LivePauseRequestDisposition::External { generation: 2 }
        );
        assert!(session_authority_devices_are_revoked(authority));
        assert_eq!(authority.activate(), None, "early activate is held");
        assert_eq!(
            authority
                .request_pause()
                .expect("duplicate pause is classified"),
            LivePauseRequestDisposition::Duplicate
        );
        assert_eq!(
            authority.complete_pause(true),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::External,
                resumable: true,
                activate_pending: true,
            })
        );
        assert_eq!(authority.complete_pause(true), None);
        assert_eq!(authority.activate(), Some(2));
        assert_eq!(authority.activate(), Some(2), "duplicate activate is stale");
    }

    #[test]
    fn external_pause_during_chord_preparation_drops_the_deferred_switch() {
        let error = KmsLiveError::AuthorityLost(LiveRevocation::SessionPause);
        let mut pending_vt_switch = None;
        assert!(
            !defer_vt_switch_after_transition_failure(&mut pending_vt_switch, 4, &error),
            "external authority loss makes the original chord stale"
        );

        let mut submitted = Vec::new();
        if let Some(vt) = pending_vt_switch.take() {
            submitted.push(vt);
        }
        assert!(
            submitted.is_empty(),
            "teardown must not submit a second change_vt after external pause"
        );

        let not_prepared =
            require_accepted_self_switch(4, VtSwitchAsk::Refused(SELF_SWITCH_NOT_PREPARED.into()))
                .expect_err("a PRE-request pause is terminal");
        assert!(is_external_authority_loss(&not_prepared));
    }

    #[test]
    fn chord_after_external_pause_is_discarded_and_the_pause_remains_resumable() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        assert_eq!(
            authority.request_pause().expect("external pause starts"),
            LivePauseRequestDisposition::External { generation: 2 }
        );

        let mut inner = SupervisorMailbox::new(
            std::iter::empty(),
            [
                Some(LiveCoordinatorEvent::VtSwitchRequested(4)),
                updated_reply(Vec::new()),
            ],
        );
        let mut mailbox = ExternalPauseMailbox::new(&mut inner, "external pausing test");
        let event = mailbox
            .poll_event()
            .expect("mailbox remains healthy")
            .expect("the pump reply follows the discarded chord");
        assert!(matches!(
            classify_transition_wait_event(event, "external pausing test")
                .expect("the discarded chord cannot become a setup error"),
            PumpReply::Updated(Ok(events)) if events.is_empty()
        ));

        assert_eq!(
            authority.complete_pause(true),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::External,
                resumable: true,
                activate_pending: false,
            })
        );
        assert_eq!(
            authority
                .begin_resume()
                .expect("the external pause can resume"),
            3
        );
    }

    fn prior_mode_for_test() -> super::super::kms::ConnectorMode {
        super::super::kms::ConnectorMode {
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            preferred: true,
            clock_khz: 148_500,
            hsync: (1920, 2008, 2200),
            vsync: (1080, 1084, 1125),
            hskew: 0,
            vscan: 0,
            flags: 5,
        }
    }

    #[test]
    fn resume_mode_filter_keeps_only_the_exact_full_timing() {
        let required = prior_mode_for_test();
        let mut different_timing = required;
        different_timing.clock_khz = 148_352;
        different_timing.flags = 9;
        let mut modes = vec![different_timing, required];

        retain_exact_prior_mode(&mut modes, required)
            .expect("the exact old timing survives a same-tuple neighbour");
        assert_eq!(modes, [required]);
    }

    #[test]
    fn resume_mode_filter_rejects_a_same_tuple_with_different_timing() {
        let required = prior_mode_for_test();
        let mut replacement = required;
        replacement.hsync = (1920, 2016, 2200);
        let mut modes = vec![replacement];

        let error = retain_exact_prior_mode(&mut modes, required)
            .expect_err("tuple equality cannot substitute for full timing identity");
        assert!(error.to_string().contains("kms-live-prior-mode-missing"));
        assert!(modes.is_empty());
    }

    #[test]
    fn activate_reopens_the_original_authority_instead_of_retaining_a_duplicate() {
        let mut authority = LiveSessionAuthority::Preparing { generation: 1 };
        let mut original = None;
        let mut opens = 0;
        let first_verification = open_authorised_session_device(
            &mut authority,
            true,
            &mut original,
            || -> Result<OwnedFd, &'static str> {
                opens += 1;
                Ok(pipe_fd())
            },
        )
        .expect("initial authority open");
        drop(first_verification);
        authority.begin_self_switch(2).expect("self-switch starts");
        authority
            .submit_self_switch()
            .expect("VT request is submitted");
        assert_eq!(authority.confirm_self_pause(), Some(2));
        close_retained_session_device(&mut original, |_fd| Ok::<(), &'static str>(()))
            .expect("the retained original closes at the paused boundary");
        assert!(original.is_none());

        assert_eq!(authority.begin_resume().expect("activate begins resume"), 3);
        let second_verification = open_authorised_session_device(
            &mut authority,
            true,
            &mut original,
            || -> Result<OwnedFd, &'static str> {
                opens += 1;
                Ok(pipe_fd())
            },
        )
        .expect("resume reopens through the authority provider");

        assert_eq!(opens, 2, "resume must invoke a second authority open");
        assert!(original.is_some(), "the newly opened original is retained");
        drop(second_verification);
    }

    #[test]
    fn self_switch_refusal_lost_reply_and_timeout_are_terminal() {
        assert_eq!(VT_SWITCH_REPLY_TIMEOUT, Duration::from_secs(1));
        for outcome in [
            VtSwitchAsk::Refused("seat refused".into()),
            VtSwitchAsk::Unsent,
            VtSwitchAsk::Dropped,
            VtSwitchAsk::TimedOut,
        ] {
            let error = require_accepted_self_switch(3, outcome)
                .expect_err("every non-acceptance is terminal after quiescence");
            assert!(error.to_string().contains("was not accepted"));
        }
        require_accepted_self_switch(3, VtSwitchAsk::Accepted)
            .expect("only an accepted switch may await the pause echo");
        let missing = missing_self_pause_confirmation(2);
        assert!(missing.to_string().contains("generation 2"));
        assert!(missing.to_string().contains("within 1000ms"));
    }

    #[test]
    fn resume_retries_only_failure_atomic_authority_open_timing() {
        for retryable in [
            KmsLiveRefusal::SessionInactiveBeforeAuthorityOpen,
            KmsLiveRefusal::DrmNodeOpenFailed,
        ] {
            assert!(resume_authority_open_is_retryable(&retryable.into()));
        }
        for terminal in [
            KmsLiveError::Refused(KmsLiveRefusal::RevokedBeforeAuthorityOpen),
            KmsLiveError::Setup("session command channel closed".into()),
            KmsLiveError::PumpDetached("render worker detached".into()),
        ] {
            assert!(!resume_authority_open_is_retryable(&terminal));
        }
    }

    #[test]
    fn failed_resume_returns_the_coordinator_reducer_to_paused() {
        for generation in [3, 4, 6, 99] {
            let mut stale = LiveCoordinatorLifecycle {
                state: LiveCoordinatorLifecycleState::Resuming { generation: 3 },
                last_submitted_at: None,
            };
            assert_eq!(
                stale
                    .apply(LiveCoordinatorLifecycleEvent::ResumeFailed { generation })
                    .expect_err("only a real suspended boundary may admit a retry")
                    .code,
                "kms-live-stale-generation"
            );
        }
        let mut lifecycle = LiveCoordinatorLifecycle {
            state: LiveCoordinatorLifecycleState::Resuming { generation: 3 },
            last_submitted_at: None,
        };
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::ResumeFailed { generation: 5 })
                .expect("compensating suspend is accepted"),
            LiveCoordinatorLifecycleAction::Paused
        );
        assert_eq!(
            lifecycle.state,
            LiveCoordinatorLifecycleState::Paused { generation: 5 }
        );
        assert_eq!(lifecycle.last_submitted_at, None);
    }

    #[test]
    fn coordinator_lifecycle_covers_pause_resume_and_rejects_stale_generations() {
        let mut lifecycle = LiveCoordinatorLifecycle::active(1, Duration::from_secs(1));
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation: 2 })
                .expect("pause begins"),
            LiveCoordinatorLifecycleAction::BeginPause
        );
        assert_eq!(lifecycle.last_submitted_at, None);
        let stale = lifecycle
            .apply(LiveCoordinatorLifecycleEvent::Suspended { generation: 1 })
            .expect_err("stale suspend rejected");
        assert_eq!(stale.code, "kms-live-stale-generation");
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::Suspended { generation: 2 })
                .expect("suspend completes"),
            LiveCoordinatorLifecycleAction::Paused
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::RequestUpdate)
                .expect("paused update decision"),
            LiveCoordinatorLifecycleAction::Hold
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginResume { generation: 3 })
                .expect("resume begins"),
            LiveCoordinatorLifecycleAction::BeginResume
        );
        assert_eq!(lifecycle.last_submitted_at, None);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::OutputReady {
                    generation: 4,
                    observed_at: Duration::from_secs(20),
                })
                .expect("resumed output ready"),
            LiveCoordinatorLifecycleAction::Active
        );
        assert_eq!(lifecycle.last_submitted_at, Some(Duration::from_secs(20)));
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::RequestUpdate)
                .expect("active update decision"),
            LiveCoordinatorLifecycleAction::IssueUpdate
        );
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::FrameSubmitted {
                generation: 4,
                observed_at: Duration::from_secs(21),
            })
            .expect("submission rearms deadline");
        assert_eq!(lifecycle.last_submitted_at, Some(Duration::from_secs(21)));
    }

    #[test]
    fn future_pause_and_resume_generations_are_rejected_without_wedging_the_next_transition() {
        let mut lifecycle = LiveCoordinatorLifecycle::active(1, Duration::ZERO);
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginPause {
                    generation: u64::MAX,
                })
                .expect_err("future pause generation rejected")
                .code,
            "kms-live-stale-generation"
        );
        assert_eq!(
            lifecycle.state,
            LiveCoordinatorLifecycleState::Active { generation: 1 }
        );
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation: 2 })
            .expect("the real next pause still begins");
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::Suspended { generation: 2 })
            .expect("the real suspend still completes");

        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginResume {
                    generation: u64::MAX,
                })
                .expect_err("future resume generation rejected")
                .code,
            "kms-live-stale-generation"
        );
        assert_eq!(
            lifecycle.state,
            LiveCoordinatorLifecycleState::Paused { generation: 2 }
        );
        assert_eq!(
            lifecycle
                .apply(LiveCoordinatorLifecycleEvent::BeginResume { generation: 3 })
                .expect("the real next resume still begins"),
            LiveCoordinatorLifecycleAction::BeginResume
        );
    }

    #[test]
    fn paused_coordinator_never_issues_an_update() {
        for state in [
            LiveCoordinatorLifecycleState::Pausing { generation: 2 },
            LiveCoordinatorLifecycleState::Paused { generation: 2 },
            LiveCoordinatorLifecycleState::Resuming { generation: 3 },
        ] {
            let mut lifecycle = LiveCoordinatorLifecycle {
                state,
                last_submitted_at: None,
            };
            assert_eq!(
                lifecycle
                    .apply(LiveCoordinatorLifecycleEvent::RequestUpdate)
                    .expect("inactive coordinator holds an update"),
                LiveCoordinatorLifecycleAction::Hold
            );
            assert_eq!(lifecycle.state, state);
            assert_eq!(lifecycle.last_submitted_at, None);
        }
    }

    #[test]
    fn no_submit_clock_is_absent_while_paused_and_rearmed_from_resume_readiness() {
        let mut lifecycle = LiveCoordinatorLifecycle::active(1, Duration::from_secs(1));
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::BeginPause { generation: 2 })
            .expect("pause begins");
        assert_eq!(lifecycle.last_submitted_at, None);
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::Suspended { generation: 2 })
            .expect("pause completes");
        assert_eq!(lifecycle.last_submitted_at, None);
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::BeginResume { generation: 3 })
            .expect("resume begins");
        assert_eq!(lifecycle.last_submitted_at, None);
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::OutputReady {
                generation: 4,
                observed_at: Duration::from_secs(20),
            })
            .expect("resume readiness arrives");
        assert_eq!(lifecycle.last_submitted_at, Some(Duration::from_secs(20)));
        lifecycle
            .apply(LiveCoordinatorLifecycleEvent::FrameSubmitted {
                generation: 4,
                observed_at: Duration::from_secs(21),
            })
            .expect("submission rearms the clock");
        assert_eq!(lifecycle.last_submitted_at, Some(Duration::from_secs(21)));
    }

    #[test]
    fn coordinator_lifecycle_transition_table_is_exhaustive() {
        let states = [
            LiveCoordinatorLifecycleState::Active { generation: 10 },
            LiveCoordinatorLifecycleState::Pausing { generation: 10 },
            LiveCoordinatorLifecycleState::Paused { generation: 10 },
            LiveCoordinatorLifecycleState::Resuming { generation: 10 },
            LiveCoordinatorLifecycleState::Terminal,
        ];
        let events = [
            LiveCoordinatorLifecycleEvent::BeginPause { generation: 11 },
            LiveCoordinatorLifecycleEvent::Suspended { generation: 10 },
            LiveCoordinatorLifecycleEvent::BeginResume { generation: 11 },
            LiveCoordinatorLifecycleEvent::OutputReady {
                generation: 11,
                observed_at: Duration::from_secs(2),
            },
            LiveCoordinatorLifecycleEvent::FrameSubmitted {
                generation: 10,
                observed_at: Duration::from_secs(2),
            },
            LiveCoordinatorLifecycleEvent::RequestUpdate,
        ];
        let expected_actions = [
            [
                Some(LiveCoordinatorLifecycleAction::BeginPause),
                None,
                None,
                None,
                Some(LiveCoordinatorLifecycleAction::Active),
                Some(LiveCoordinatorLifecycleAction::IssueUpdate),
            ],
            [
                None,
                Some(LiveCoordinatorLifecycleAction::Paused),
                None,
                None,
                None,
                Some(LiveCoordinatorLifecycleAction::Hold),
            ],
            [
                None,
                None,
                Some(LiveCoordinatorLifecycleAction::BeginResume),
                None,
                None,
                Some(LiveCoordinatorLifecycleAction::Hold),
            ],
            [
                None,
                None,
                None,
                Some(LiveCoordinatorLifecycleAction::Active),
                None,
                Some(LiveCoordinatorLifecycleAction::Hold),
            ],
            [
                Some(LiveCoordinatorLifecycleAction::Terminal),
                Some(LiveCoordinatorLifecycleAction::Terminal),
                Some(LiveCoordinatorLifecycleAction::Terminal),
                Some(LiveCoordinatorLifecycleAction::Terminal),
                Some(LiveCoordinatorLifecycleAction::Terminal),
                Some(LiveCoordinatorLifecycleAction::Terminal),
            ],
        ];

        for (state_index, state) in states.into_iter().enumerate() {
            for (event_index, event) in events.into_iter().enumerate() {
                let initial_clock = matches!(state, LiveCoordinatorLifecycleState::Active { .. })
                    .then_some(Duration::ZERO);
                let mut lifecycle = LiveCoordinatorLifecycle {
                    state,
                    last_submitted_at: initial_clock,
                };
                let result = lifecycle.apply(event);
                let Some(expected_action) = expected_actions[state_index][event_index] else {
                    assert!(result.is_err(), "state {state:?}, event {event:?}");
                    assert_eq!(lifecycle.state, state);
                    assert_eq!(lifecycle.last_submitted_at, initial_clock);
                    continue;
                };
                assert_eq!(
                    result.expect("accepted table entry"),
                    expected_action,
                    "state {state:?}, event {event:?}"
                );
                let expected_state = match expected_action {
                    LiveCoordinatorLifecycleAction::BeginPause => {
                        LiveCoordinatorLifecycleState::Pausing { generation: 11 }
                    }
                    LiveCoordinatorLifecycleAction::Paused => {
                        LiveCoordinatorLifecycleState::Paused { generation: 10 }
                    }
                    LiveCoordinatorLifecycleAction::BeginResume => {
                        LiveCoordinatorLifecycleState::Resuming { generation: 11 }
                    }
                    LiveCoordinatorLifecycleAction::Active if event_index == 3 => {
                        LiveCoordinatorLifecycleState::Active { generation: 11 }
                    }
                    LiveCoordinatorLifecycleAction::Active
                    | LiveCoordinatorLifecycleAction::IssueUpdate
                    | LiveCoordinatorLifecycleAction::Hold => state,
                    LiveCoordinatorLifecycleAction::Terminal => {
                        LiveCoordinatorLifecycleState::Terminal
                    }
                };
                assert_eq!(lifecycle.state, expected_state);
                let expected_clock = match expected_action {
                    LiveCoordinatorLifecycleAction::Active if event_index == 4 => {
                        Some(Duration::from_secs(2))
                    }
                    LiveCoordinatorLifecycleAction::Active if event_index == 3 => {
                        Some(Duration::from_secs(2))
                    }
                    LiveCoordinatorLifecycleAction::IssueUpdate => initial_clock,
                    LiveCoordinatorLifecycleAction::BeginPause
                    | LiveCoordinatorLifecycleAction::Paused
                    | LiveCoordinatorLifecycleAction::BeginResume
                    | LiveCoordinatorLifecycleAction::Hold
                    | LiveCoordinatorLifecycleAction::Terminal
                    | LiveCoordinatorLifecycleAction::Active => None,
                };
                assert_eq!(lifecycle.last_submitted_at, expected_clock);
            }
        }
    }

    #[test]
    fn every_cyclic_transition_rejects_a_stale_or_mismatched_generation() {
        let cases = [
            (
                LiveCoordinatorLifecycleState::Active { generation: 10 },
                LiveCoordinatorLifecycleEvent::BeginPause { generation: 10 },
            ),
            (
                LiveCoordinatorLifecycleState::Pausing { generation: 10 },
                LiveCoordinatorLifecycleEvent::Suspended { generation: 9 },
            ),
            (
                LiveCoordinatorLifecycleState::Paused { generation: 10 },
                LiveCoordinatorLifecycleEvent::BeginResume { generation: 10 },
            ),
            (
                LiveCoordinatorLifecycleState::Resuming { generation: 10 },
                LiveCoordinatorLifecycleEvent::OutputReady {
                    generation: 9,
                    observed_at: Duration::ZERO,
                },
            ),
            (
                LiveCoordinatorLifecycleState::Resuming { generation: 10 },
                LiveCoordinatorLifecycleEvent::OutputReady {
                    generation: 12,
                    observed_at: Duration::ZERO,
                },
            ),
            (
                LiveCoordinatorLifecycleState::Active { generation: 10 },
                LiveCoordinatorLifecycleEvent::FrameSubmitted {
                    generation: 9,
                    observed_at: Duration::ZERO,
                },
            ),
        ];
        for (state, event) in cases {
            let mut lifecycle = LiveCoordinatorLifecycle {
                state,
                last_submitted_at: Some(Duration::ZERO),
            };
            assert_eq!(
                lifecycle
                    .apply(event)
                    .expect_err("stale event rejected")
                    .code,
                "kms-live-stale-generation"
            );
        }
    }

    #[test]
    fn signal_fatal_and_detach_are_terminal_from_every_lifecycle_state() {
        let states = [
            LiveCoordinatorLifecycleState::Active { generation: 1 },
            LiveCoordinatorLifecycleState::Pausing { generation: 2 },
            LiveCoordinatorLifecycleState::Paused { generation: 2 },
            LiveCoordinatorLifecycleState::Resuming { generation: 3 },
            LiveCoordinatorLifecycleState::Terminal,
        ];
        for state in states {
            for event in [
                LiveCoordinatorLifecycleEvent::Signal,
                LiveCoordinatorLifecycleEvent::Fatal,
                LiveCoordinatorLifecycleEvent::PumpDetached,
            ] {
                let mut lifecycle = LiveCoordinatorLifecycle {
                    state,
                    last_submitted_at: Some(Duration::ZERO),
                };
                assert_eq!(
                    lifecycle.apply(event).expect("terminal event accepted"),
                    LiveCoordinatorLifecycleAction::Terminal
                );
                assert_eq!(lifecycle.state, LiveCoordinatorLifecycleState::Terminal);
                assert_eq!(lifecycle.last_submitted_at, None);
                assert_eq!(
                    lifecycle
                        .apply(LiveCoordinatorLifecycleEvent::BeginResume { generation: 99 })
                        .expect("terminal state cannot resume"),
                    LiveCoordinatorLifecycleAction::Terminal
                );
            }
        }
    }

    #[test]
    fn live_target_pairing_distinguishes_missing_active_and_released_generations() {
        let ledger = LiveTargetPairingLedger::default();
        assert!(!ledger.snapshot(1).is_paired());

        ledger.record_created(1);
        assert_eq!(
            ledger.snapshot(1),
            LiveTargetPairingCounts {
                created: 1,
                released: 0,
            }
        );
        assert!(!ledger.snapshot(1).is_paired());

        ledger.record_released(1);
        ledger.record_created(3);
        assert!(ledger.snapshot(1).is_paired());
        assert_eq!(
            ledger.inactive_snapshot(3),
            LiveTargetPairingCounts {
                created: 1,
                released: 1,
            },
            "the active generation is excluded from completed-cycle pairing"
        );
    }

    #[test]
    fn retained_storage_accounting_never_falsifies_target_pairing_counts() {
        let ledger = LiveTargetPairingLedger::default();
        ledger.record_created(5);
        ledger.record_released(5);
        ledger.record_retained_created(5);

        assert_eq!(
            ledger.snapshot(5),
            LiveTargetPairingCounts {
                created: 1,
                released: 1,
            }
        );
        assert_eq!(
            ledger.retained_snapshot(5),
            LiveRetainedBufferPairingCounts {
                created: 1,
                released: 0,
                pending_handoffs: 0,
            }
        );
        ledger.record_retained_handoff_started(5);
        assert!(ledger.retained_snapshot(5).pending_handoff());
        assert!(ledger.retained_snapshot(5).is_healthy_while_active());
        ledger.record_retained_released(5, true);
        assert_eq!(
            ledger.retained_snapshot(5),
            LiveRetainedBufferPairingCounts {
                created: 1,
                released: 1,
                pending_handoffs: 0,
            }
        );
    }

    #[test]
    fn live_session_wait_bounds_distinguish_cold_readiness_from_warm_commands() {
        assert_eq!(INPUT_OPEN_TIMEOUT, Duration::from_secs(1));
        assert_eq!(INPUT_CLOSE_TIMEOUT, Duration::from_secs(1));
        assert_eq!(SESSION_SHUTDOWN_TIMEOUT, Duration::from_secs(1));
        assert_eq!(INITIAL_SESSION_READINESS_TIMEOUT, Duration::from_secs(15));
        assert_eq!(RUNNING_SESSION_COMMAND_TIMEOUT, Duration::from_secs(3));
    }

    struct FakeLiveActPlatform {
        events: Vec<&'static str>,
        fail_at: Vec<&'static str>,
        /// What the revocation wait reports, when it is reached.
        revoke_with: SessionTeardown,
        adapter_start: AdapterStartDecision,
        before_protocol_revocation: Option<LiveRevocation>,
        queued_revocations: Vec<LiveRevocation>,
        vt_switch: Option<u8>,
        unstarted_pump: bool,
        external_pause_pending: bool,
        pause_after_vt_switch: bool,
        /// What the teardown funnel actually asked for, so a test can pin that
        /// the wait's answer is the one that arrives — rather than a constant.
        closed_with: Option<SessionTeardown>,
    }

    struct RealPumpSwitchOrderPlatform {
        switch_vt: mpsc::SyncSender<()>,
    }

    impl LiveActPlatform for RealPumpSwitchOrderPlatform {
        type Lease = ();
        type SelectedTarget = ();
        type Protocol = ();
        type Adapter = super::super::render::LiveRenderPump;

        fn open_authorised_device(&mut self, _device_path: &Path) -> Result<OwnedFd, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn duplicate_lease(&mut self) -> Result<Self::Lease, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn discard_verification_fd(&mut self, _verified: &mut VerifiedDrmFd) {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn select_target(
            &mut self,
            _verified: &VerifiedDrmFd,
        ) -> Result<Self::SelectedTarget, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn start_protocol(
            &mut self,
            _target: &Self::SelectedTarget,
        ) -> Result<Self::Protocol, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn adapter_start_decision(&mut self) -> AdapterStartDecision {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn start_adapter(
            &mut self,
            _lease: Self::Lease,
            _target: Self::SelectedTarget,
        ) -> Result<Self::Adapter, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn wait_for_revocation(
            &mut self,
            _adapter: &mut Self::Adapter,
            _verified: &VerifiedDrmFd,
            _grant: &KmsLiveGrant,
        ) -> Result<SessionTeardown, KmsLiveError> {
            unreachable!("the ordering test enters at the teardown funnel")
        }

        fn shutdown_adapter(&mut self, adapter: Self::Adapter) -> Result<(), KmsLiveError> {
            adapter.shutdown()
        }

        fn after_adapter_shutdown(&mut self) -> Result<(), KmsLiveError> {
            self.switch_vt
                .send(())
                .map_err(|_| KmsLiveError::Setup("VT-switch observer disconnected".into()))
        }

        fn stop_protocol(&mut self, _protocol: Self::Protocol) {}

        fn close_session(&mut self, _teardown: SessionTeardown) -> Result<(), KmsLiveError> {
            Ok(())
        }
    }

    struct BoundaryActPlatform {
        grant_platform: Rc<FakePlatform>,
    }

    impl LiveActPlatform for BoundaryActPlatform {
        type Lease = ();
        type SelectedTarget = ();
        type Protocol = ();
        type Adapter = ();

        fn open_authorised_device(&mut self, _device_path: &Path) -> Result<OwnedFd, KmsLiveError> {
            self.grant_platform
                .boundary_events
                .borrow_mut()
                .push("open");
            self.grant_platform
                .drm_open_count
                .set(self.grant_platform.drm_open_count.get().saturating_add(1));
            if let Some(error) = self.grant_platform.drm_open_error.get() {
                return Err(error.into());
            }
            Ok(harmless_fd())
        }

        fn duplicate_lease(&mut self) -> Result<Self::Lease, KmsLiveError> {
            Ok(())
        }

        fn discard_verification_fd(&mut self, verified: &mut VerifiedDrmFd) {
            drop(verified.fd.take());
        }

        fn select_target(
            &mut self,
            _verified: &VerifiedDrmFd,
        ) -> Result<Self::SelectedTarget, KmsLiveError> {
            Err(KmsLiveRefusal::LiveBodyUnavailable.into())
        }

        fn start_protocol(
            &mut self,
            (): &Self::SelectedTarget,
        ) -> Result<Self::Protocol, KmsLiveError> {
            unreachable!("the offline boundary stops before protocol startup")
        }

        fn adapter_start_decision(&mut self) -> AdapterStartDecision {
            unreachable!("the offline boundary stops before protocol startup")
        }

        fn start_adapter(
            &mut self,
            (): Self::Lease,
            (): Self::SelectedTarget,
        ) -> Result<Self::Adapter, KmsLiveError> {
            unreachable!("the offline boundary stops before adapter startup")
        }

        fn wait_for_revocation(
            &mut self,
            (): &mut Self::Adapter,
            _verified: &VerifiedDrmFd,
            _grant: &KmsLiveGrant,
        ) -> Result<SessionTeardown, KmsLiveError> {
            unreachable!("the offline boundary installs no adapter")
        }

        fn shutdown_adapter(&mut self, (): Self::Adapter) -> Result<(), KmsLiveError> {
            Ok(())
        }

        fn stop_protocol(&mut self, (): Self::Protocol) {}

        fn close_session(&mut self, _teardown: SessionTeardown) -> Result<(), KmsLiveError> {
            Ok(())
        }
    }

    fn execute_verified_test(
        grant: KmsLiveGrant,
        grant_platform: Rc<FakePlatform>,
    ) -> Result<(), KmsLiveError> {
        operate_verified_with(&mut BoundaryActPlatform { grant_platform }, grant)
    }

    impl FakeLiveActPlatform {
        fn new(fail_at: Option<&'static str>) -> Self {
            Self {
                events: Vec::new(),
                fail_at: fail_at.into_iter().collect(),
                revoke_with: SessionTeardown::Graceful,
                adapter_start: AdapterStartDecision::Start,
                before_protocol_revocation: None,
                queued_revocations: Vec::new(),
                vt_switch: None,
                unstarted_pump: false,
                external_pause_pending: false,
                pause_after_vt_switch: false,
                closed_with: None,
            }
        }

        fn failing_at(fail_at: &[&'static str]) -> Self {
            Self {
                events: Vec::new(),
                fail_at: fail_at.to_vec(),
                revoke_with: SessionTeardown::Graceful,
                adapter_start: AdapterStartDecision::Start,
                before_protocol_revocation: None,
                queued_revocations: Vec::new(),
                vt_switch: None,
                unstarted_pump: false,
                external_pause_pending: false,
                pause_after_vt_switch: false,
                closed_with: None,
            }
        }

        fn revoking_with(teardown: SessionTeardown) -> Self {
            Self {
                revoke_with: teardown,
                ..Self::new(None)
            }
        }

        fn ending_before_adapter_with(revocation: LiveRevocation) -> Self {
            Self {
                adapter_start: adapter_start_after_revocations(&[revocation]),
                queued_revocations: vec![revocation],
                ..Self::new(None)
            }
        }

        fn signalling_before_adapter(signal: LiveSignal) -> Self {
            let queued = [LiveCoordinatorEvent::Signal(signal)];
            Self {
                adapter_start: adapter_start_after_events(&queued, None),
                ..Self::new(None)
            }
        }

        fn switching_vt(vt: u8) -> Self {
            Self {
                vt_switch: Some(vt),
                ..Self::new(None)
            }
        }

        fn switching_before_adapter(vt: u8) -> Self {
            Self {
                adapter_start: AdapterStartDecision::EndVtSwitch(vt),
                vt_switch: Some(vt),
                unstarted_pump: true,
                ..Self::new(None)
            }
        }

        fn pausing_before_protocol() -> Self {
            Self {
                before_protocol_revocation: Some(LiveRevocation::SessionPause),
                unstarted_pump: true,
                external_pause_pending: true,
                ..Self::new(None)
            }
        }

        fn pausing_before_adapter() -> Self {
            Self {
                adapter_start: AdapterStartDecision::EndAuthority(LiveRevocation::SessionPause),
                unstarted_pump: true,
                external_pause_pending: true,
                ..Self::new(None)
            }
        }

        fn event(&mut self, event: &'static str) -> Result<(), KmsLiveError> {
            self.events.push(event);
            if self.fail_at.contains(&event) {
                Err(KmsLiveError::Setup(format!("injected {event} failure")))
            } else {
                Ok(())
            }
        }
    }

    impl LiveActPlatform for FakeLiveActPlatform {
        type Lease = ();
        type SelectedTarget = ();
        type Protocol = ();
        type Adapter = ();

        fn before_authority_open(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
            if self.fail_at.contains(&"before-authority-open") {
                self.event("before-authority-open").map(|()| None)
            } else {
                Ok(None)
            }
        }

        fn open_authorised_device(&mut self, _device_path: &Path) -> Result<OwnedFd, KmsLiveError> {
            self.event("authority-open")?;
            Ok(harmless_fd())
        }

        fn duplicate_lease(&mut self) -> Result<Self::Lease, KmsLiveError> {
            self.event("duplicate-lease")
        }

        fn discard_verification_fd(&mut self, verified: &mut VerifiedDrmFd) {
            self.events.push("discard-verification");
            drop(verified.fd.take());
        }

        fn select_target(
            &mut self,
            _verified: &VerifiedDrmFd,
        ) -> Result<Self::SelectedTarget, KmsLiveError> {
            self.event("select-target")
        }

        fn start_protocol(
            &mut self,
            (): &Self::SelectedTarget,
        ) -> Result<Self::Protocol, KmsLiveError> {
            self.event("start-protocol")
        }

        fn before_protocol_start(&mut self) -> Result<Option<LiveRevocation>, KmsLiveError> {
            if self.fail_at.contains(&"before-protocol-start") {
                self.event("before-protocol-start").map(|()| None)
            } else if let Some(revocation) = self.before_protocol_revocation {
                self.events.push("before-protocol-start");
                Ok(Some(revocation))
            } else {
                Ok(None)
            }
        }

        fn adapter_start_decision(&mut self) -> AdapterStartDecision {
            self.events.push("decide-adapter");
            self.adapter_start
        }

        fn start_adapter(
            &mut self,
            (): Self::Lease,
            (): Self::SelectedTarget,
        ) -> Result<Self::Adapter, KmsLiveError> {
            self.event("start-adapter")
        }

        fn wait_for_revocation(
            &mut self,
            (): &mut Self::Adapter,
            _verified: &VerifiedDrmFd,
            _grant: &KmsLiveGrant,
        ) -> Result<SessionTeardown, KmsLiveError> {
            self.event("wait-revocation")?;
            Ok(self.revoke_with)
        }

        fn shutdown_adapter(&mut self, (): Self::Adapter) -> Result<(), KmsLiveError> {
            self.event("shutdown-adapter")
        }

        fn after_adapter_shutdown(&mut self) -> Result<(), KmsLiveError> {
            if self.unstarted_pump {
                self.events.push("shutdown-unstarted-pump");
            }
            if self.external_pause_pending {
                if self.events.contains(&"start-protocol") {
                    self.events.push("reconcile-input");
                }
                self.events.push("close-original");
                self.events.push("acknowledge-external-pause");
            }
            if self.vt_switch.is_some() {
                self.events.push("switch-vt");
                if self.pause_after_vt_switch {
                    self.queued_revocations.push(LiveRevocation::SessionPause);
                }
            }
            Ok(())
        }

        fn stop_protocol(&mut self, (): Self::Protocol) {
            self.events.push("stop-protocol");
        }

        fn close_session(&mut self, teardown: SessionTeardown) -> Result<(), KmsLiveError> {
            // The production composition, not a transcription of it: the fake
            // holds its queued revocations in a plain field where production
            // drains a channel, but the decision both take from that evidence
            // is the same function.
            let (upgraded, decision_failure) =
                resolve_session_close(teardown, &self.queued_revocations);
            self.closed_with = Some(upgraded);
            combine_live_results(self.event("close-session"), decision_failure)
        }
    }

    fn grant_for_act_test() -> KmsLiveGrant {
        ordinary_grant(Rc::new(FakePlatform::new([vt(), vt(), vt(), vt()])))
            .expect("fake act grant")
    }

    #[test]
    fn only_an_unresponsive_session_is_torn_down_by_detaching() {
        // A legacy pause revocation (only possible outside active supervision)
        // and hotplug are ordinary terminal paths: the session thread is
        // answering, so it can be told to stop and waited for. Active external
        // pauses use `PauseRequested` and do not reach this reducer.
        assert_eq!(
            session_teardown_after(Some(LiveRevocation::SessionPause)),
            SessionTeardown::Graceful
        );
        assert_eq!(
            session_teardown_after(Some(LiveRevocation::TargetHotplug)),
            SessionTeardown::Graceful
        );
        assert_eq!(
            session_teardown_after(None),
            SessionTeardown::Graceful,
            "an operation that never reached the wait has no reason to detach"
        );
        assert_eq!(
            session_teardown_after(Some(LiveRevocation::SessionThreadStopped)),
            SessionTeardown::Graceful,
            "a thread that has already left answers join immediately, so there is nothing to \
             detach from"
        );
        assert_eq!(
            session_teardown_after(Some(LiveRevocation::ProtocolThreadStopped)),
            SessionTeardown::Graceful,
            "a protocol failure ends the operation but does not make the independent session \
             thread unsafe to shut down"
        );
        // The one case where the ordinary route is already known to be pointless:
        // a thread that missed its open deadline will miss the shutdown deadline
        // too, and detaching is where that ends up regardless.
        assert_eq!(
            session_teardown_after(Some(LiveRevocation::SessionUnresponsive)),
            SessionTeardown::Detach
        );
    }

    #[test]
    fn the_close_decision_composition_is_pinned_arm_by_arm() {
        // Nothing queued: the chosen teardown stands and carries no failure.
        let (upgraded, failure) = resolve_session_close(SessionTeardown::Graceful, &[]);
        assert_eq!(upgraded, SessionTeardown::Graceful);
        assert!(failure.is_ok());

        // A queued fatal upgrades a graceful choice, and the upgrade itself is
        // a failure — the abandoned thread leaked what it held.
        let (upgraded, failure) = resolve_session_close(
            SessionTeardown::Graceful,
            &[LiveRevocation::SessionUnresponsive],
        );
        assert_eq!(upgraded, SessionTeardown::Detach);
        let error = failure.expect_err("an upgraded detach is not success");
        assert!(error.to_string().contains("abandoned"), "{error}");

        // A queued protocol failure does not change how the healthy session
        // thread closes, but it must still fail the process result.
        let (upgraded, failure) = resolve_session_close(
            SessionTeardown::Graceful,
            &[LiveRevocation::ProtocolThreadStopped],
        );
        assert_eq!(upgraded, SessionTeardown::Graceful);
        let error = failure.expect_err("a queued protocol failure is not success");
        assert!(
            error.to_string().contains("protocol thread stopped"),
            "{error}"
        );

        // A detach the coordinator already chose is not re-reported by the
        // upgrade rule; its failure is `unresponsive_is_not_success`'s, in
        // `finish_live_operation`.
        let (upgraded, failure) = resolve_session_close(
            SessionTeardown::Detach,
            &[LiveRevocation::SessionUnresponsive],
        );
        assert_eq!(upgraded, SessionTeardown::Detach);
        assert!(failure.is_ok());

        // Both failures at once are both reported.
        let (upgraded, failure) = resolve_session_close(
            SessionTeardown::Graceful,
            &[
                LiveRevocation::ProtocolThreadStopped,
                LiveRevocation::SessionUnresponsive,
            ],
        );
        assert_eq!(upgraded, SessionTeardown::Detach);
        let error = failure.expect_err("two queued failures are not success");
        let message = error.to_string();
        assert!(
            message.contains("protocol thread stopped") && message.contains("abandoned"),
            "{message}"
        );
    }

    #[test]
    fn any_revocation_observed_after_protocol_start_refuses_the_render_adapter() {
        assert_eq!(
            adapter_start_after_revocations(&[]),
            AdapterStartDecision::Start
        );
        for revocation in [LiveRevocation::SessionPause, LiveRevocation::TargetHotplug] {
            assert_eq!(
                adapter_start_after_revocations(&[revocation]),
                AdapterStartDecision::EndAuthority(revocation),
                "an operation that has already been told to end must not start the adapter: \
                 {revocation:?}"
            );
        }
        for revocation in [
            LiveRevocation::SessionUnresponsive,
            LiveRevocation::SessionThreadStopped,
            LiveRevocation::ProtocolThreadStopped,
        ] {
            assert_eq!(
                adapter_start_after_revocations(&[revocation]),
                AdapterStartDecision::RefuseInternal(revocation),
                "an internal failure must refuse adapter startup: {revocation:?}"
            );
        }
        assert_eq!(
            adapter_start_after_revocations(&[
                LiveRevocation::SessionPause,
                LiveRevocation::SessionUnresponsive,
            ]),
            AdapterStartDecision::RefuseInternal(LiveRevocation::SessionUnresponsive),
            "a fatal in the bounded startup prefix must dominate an authority revocation before it"
        );
    }

    #[test]
    fn a_queued_unresponsive_revocation_overrides_a_graceful_choice() {
        // The coordinator reads the revocations channel exactly once. A pause
        // published in the same dispatch round as a stalled input open wins that
        // read, and the fatal notification is still sitting behind it — so the
        // decision has to be re-taken against what is left, or a graceful
        // teardown goes on to wait for a thread that has stopped answering.
        assert_eq!(
            teardown_upgraded_by(
                SessionTeardown::Graceful,
                [LiveRevocation::SessionUnresponsive]
            ),
            SessionTeardown::Detach,
            "a queued fatal notification must override the message that woke the coordinator"
        );
        assert_eq!(
            teardown_upgraded_by(
                SessionTeardown::Graceful,
                [
                    LiveRevocation::SessionPause,
                    LiveRevocation::TargetHotplug,
                    LiveRevocation::SessionUnresponsive,
                    LiveRevocation::SessionThreadStopped,
                ]
            ),
            SessionTeardown::Detach,
            "position in the queue must not decide it; one unresponsive anywhere is enough"
        );
        assert_eq!(
            teardown_upgraded_by(
                SessionTeardown::Graceful,
                [
                    LiveRevocation::SessionPause,
                    LiveRevocation::TargetHotplug,
                    LiveRevocation::SessionThreadStopped,
                ]
            ),
            SessionTeardown::Graceful,
            "ordinary revocations must not turn an ordinary shutdown into a leaked thread"
        );
        assert_eq!(
            teardown_upgraded_by(SessionTeardown::Graceful, []),
            SessionTeardown::Graceful,
            "an empty channel changes nothing"
        );
        assert_eq!(
            teardown_upgraded_by(SessionTeardown::Detach, [LiveRevocation::SessionPause]),
            SessionTeardown::Detach,
            "a pause arriving after the thread stopped answering does not make it answer"
        );
    }

    #[test]
    fn only_an_acknowledged_shutdown_leaves_the_thread_joinable() {
        assert_eq!(
            session_exit_after_shutdown(ShutdownAsk::Acknowledged),
            SessionExit::Joinable,
            "the acknowledgement is published after the event loop, and with it the libseat \
             notifier that owns the seat, has been destroyed"
        );
        // The three silences. None of them distinguishes a thread that has
        // finished from one stuck inside libseat, and only the first of them
        // used to be treated as a wedge.
        assert_eq!(
            session_exit_after_shutdown(ShutdownAsk::TimedOut),
            SessionExit::Wedged,
            "an alive, silent thread is exactly the one join cannot survive"
        );
        assert_eq!(
            session_exit_after_shutdown(ShutdownAsk::Dropped),
            SessionExit::Wedged,
            "a dropped reply channel says the thread stopped talking, not that it stopped \
             running: the seat close happens after the event loop that owns the reply"
        );
        assert_eq!(
            session_exit_after_shutdown(ShutdownAsk::Unsent),
            SessionExit::Wedged,
            "a command source that is already gone says nothing about the foreign close that \
             runs when the same event loop is destroyed"
        );
    }

    #[test]
    fn startup_waits_proceed_revoke_on_timeout_and_name_a_lost_channel() {
        assert_eq!(
            INITIAL_SESSION_READINESS_TIMEOUT,
            Duration::from_secs(15),
            "the unquantified cold activation retains the generous readiness deadline"
        );
        assert_eq!(
            RUNNING_SESSION_COMMAND_TIMEOUT,
            Duration::from_secs(3),
            "later session commands use the measured warm deadline"
        );

        let (reply, result) = startup_reply_channel();
        reply.send(41).expect("the one-slot reply is available");
        assert_eq!(
            classify_startup_wait(result.recv_timeout(INITIAL_SESSION_READINESS_TIMEOUT)),
            StartupWait::Proceed(41)
        );

        let (_reply, result) = startup_reply_channel::<()>();
        assert_eq!(
            classify_startup_wait(result.recv_timeout(Duration::ZERO)),
            StartupWait::TimeoutWithRevocation(LiveRevocation::SessionUnresponsive),
            "a silent live sender must request the detach-producing revocation"
        );

        let (reply, result) = startup_reply_channel::<()>();
        drop(reply);
        assert_eq!(
            classify_startup_wait(result.recv_timeout(RUNNING_SESSION_COMMAND_TIMEOUT)),
            StartupWait::LostChannel,
            "sender loss and a live-but-silent session are distinct evidence"
        );
    }

    #[test]
    fn a_shutdown_wait_is_classified_by_how_it_ended_and_carries_the_reason() {
        let (reply, result) = mpsc::sync_channel(1);
        reply.send(Ok(())).expect("the receiver is alive");
        let (ask, close) = shutdown_ask_after_wait(result.recv_timeout(SESSION_SHUTDOWN_TIMEOUT));
        assert_eq!(ask, ShutdownAsk::Acknowledged);
        assert!(close.is_ok(), "a clean close must be reported as one");

        let (reply, result) = mpsc::sync_channel(1);
        reply
            .send(Err("device close failed".into()))
            .expect("the receiver is alive");
        let (ask, close) = shutdown_ask_after_wait(result.recv_timeout(SESSION_SHUTDOWN_TIMEOUT));
        assert_eq!(
            ask,
            ShutdownAsk::Acknowledged,
            "a thread that answers has finished its teardown even when the teardown failed"
        );
        assert!(
            close
                .expect_err("a failed device close is not success")
                .to_string()
                .contains("device close failed"),
            "the thread's own reason must survive the classification"
        );

        let (reply, result) = mpsc::sync_channel::<Result<(), String>>(1);
        drop(reply);
        let (ask, close) = shutdown_ask_after_wait(result.recv_timeout(SESSION_SHUTDOWN_TIMEOUT));
        assert_eq!(ask, ShutdownAsk::Dropped);
        assert!(
            close
                .expect_err("a lost reply is not success")
                .to_string()
                .contains("reply was lost")
        );

        let (_reply, result) = mpsc::sync_channel::<Result<(), String>>(1);
        // Not `SESSION_SHUTDOWN_TIMEOUT`: the classification is of the error,
        // and spending the real deadline to obtain the same error would add
        // one second to every run of the suite.
        let (ask, close) = shutdown_ask_after_wait(result.recv_timeout(Duration::from_millis(1)));
        assert_eq!(ask, ShutdownAsk::TimedOut);
        let error = close
            .expect_err("an unanswered shutdown is not success")
            .to_string();
        assert!(
            error.contains(&format!("within {}s", SESSION_SHUTDOWN_TIMEOUT.as_secs())),
            "the failure must name the deadline it exceeded: {error}"
        );
    }

    #[test]
    fn an_input_close_wait_preserves_completion_failure_and_silence() {
        assert_eq!(
            INPUT_CLOSE_TIMEOUT,
            Duration::from_secs(1),
            "the live log and the wait must describe the same evidence-based deadline"
        );
        assert_eq!(
            classify_input_close_wait(Ok(Ok(()))),
            InputCloseWait::Closed
        );
        assert_eq!(
            classify_input_close_wait(Ok(Err("close failed".into()))),
            InputCloseWait::CloseFailed("close failed".into())
        );
        assert_eq!(
            classify_input_close_wait(Err(mpsc::RecvTimeoutError::Disconnected)),
            InputCloseWait::WaitFailed(mpsc::RecvTimeoutError::Disconnected),
            "a departed session and a live-but-wedged one are distinct evidence"
        );
        assert_eq!(
            classify_input_close_wait(Err(mpsc::RecvTimeoutError::Timeout)),
            InputCloseWait::WaitFailed(mpsc::RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn the_revocation_drain_reports_saturation_only_when_more_was_left() {
        let (sender, revocations) = mpsc::channel();
        let (queued, saturated) = drain_revocations(&revocations, 4);
        assert!(queued.is_empty());
        assert!(!saturated, "an empty channel is not a saturated one");

        for _ in 0..4 {
            sender.send(LiveRevocation::SessionPause).expect("queued");
        }
        let (queued, saturated) = drain_revocations(&revocations, 4);
        assert_eq!(queued.len(), 4);
        assert!(
            !saturated,
            "exactly the limit was drained completely; reporting a prefix decision there is \
             reporting one that was not taken"
        );

        for _ in 0..4 {
            sender.send(LiveRevocation::SessionPause).expect("queued");
        }
        sender
            .send(LiveRevocation::SessionUnresponsive)
            .expect("queued");
        let (queued, saturated) = drain_revocations(&revocations, 4);
        assert!(saturated, "one past the limit is what saturation means");
        assert!(
            queued.contains(&LiveRevocation::SessionUnresponsive),
            "the message that proved the queue was longer must still reach the decision, not be \
             discarded to keep the count round"
        );
        assert_eq!(
            teardown_upgraded_by(SessionTeardown::Graceful, queued),
            SessionTeardown::Detach,
            "which is the whole reason it is kept"
        );

        // The real limit, so the constant the live teardown passes is one a
        // test has driven to its boundary rather than one only a small
        // stand-in has.
        for _ in 0..LIVE_REVOCATION_DRAIN_LIMIT {
            sender.send(LiveRevocation::SessionPause).expect("queued");
        }
        let (queued, saturated) = drain_revocations(&revocations, LIVE_REVOCATION_DRAIN_LIMIT);
        assert_eq!(queued.len(), LIVE_REVOCATION_DRAIN_LIMIT);
        assert!(!saturated);

        // More than one past the limit, which is the case that pins the bound
        // itself. Every case above is also satisfied by a drain that simply
        // empties the channel; this one is not.
        for _ in 0..7 {
            sender.send(LiveRevocation::SessionPause).expect("queued");
        }
        let (queued, saturated) = drain_revocations(&revocations, 4);
        assert!(saturated);
        assert_eq!(
            queued.len(),
            5,
            "the drain reads the limit, then the one message that establishes there was more, and \
             stops there"
        );
        assert!(
            matches!(revocations.try_recv(), Ok(LiveRevocation::SessionPause)),
            "what the drain did not read must still be in the channel; a drain that empties it is \
             an unbounded one by another name"
        );
    }

    #[test]
    fn only_a_teardown_the_queue_upgraded_is_reported_again() {
        assert!(
            upgraded_detach_is_not_success(SessionTeardown::Graceful, SessionTeardown::Graceful)
                .is_ok()
        );
        assert!(
            upgraded_detach_is_not_success(SessionTeardown::Detach, SessionTeardown::Detach)
                .is_ok(),
            "a teardown that was already a detach when the coordinator chose it has been reported \
             by `unresponsive_is_not_success`; reporting it here as well would nest one \
             description of a single event inside another"
        );
        assert!(
            upgraded_detach_is_not_success(SessionTeardown::Detach, SessionTeardown::Graceful)
                .is_ok()
        );

        let error =
            upgraded_detach_is_not_success(SessionTeardown::Graceful, SessionTeardown::Detach)
                .expect_err(
                    "the operation's result was fixed from the graceful choice before the queue \
                     was drained, so nothing else will ever report the fatal notification that \
                     overrode it",
                );
        assert!(
            error.to_string().contains("still queued"),
            "the failure must name what overrode the choice: {error}"
        );
        assert!(
            error.to_string().contains("leaked"),
            "and what abandoning the thread cost: {error}"
        );
    }

    #[test]
    fn a_protocol_failure_found_during_teardown_reaches_the_exit_status() {
        assert!(queued_protocol_failure_is_not_success(&[]).is_ok());
        assert!(
            queued_protocol_failure_is_not_success(&[LiveRevocation::SessionPause]).is_ok(),
            "an authority revocation remains an ordinary graceful end"
        );
        let error = queued_protocol_failure_is_not_success(&[
            LiveRevocation::SessionPause,
            LiveRevocation::ProtocolThreadStopped,
        ])
        .expect_err("a protocol failure queued behind an authority revocation is not success");
        assert!(
            error.to_string().contains("protocol thread stopped"),
            "{error}"
        );
        assert_eq!(
            teardown_upgraded_by(
                SessionTeardown::Graceful,
                [LiveRevocation::ProtocolThreadStopped]
            ),
            SessionTeardown::Graceful,
            "the failure changes the process result, not the healthy session's teardown mode"
        );
    }

    #[test]
    fn the_coordinator_is_told_of_an_unanswered_open_once_and_only_on_the_transition() {
        let (fatal, revocations) = mpsc::channel();
        let mut gate = InputOpenGate::default();
        let path = Path::new("/dev/input/event0");

        // A session that is gone is not a session that stopped answering.
        let gone = refuse_input_open_after_wait_failure(
            &mut gate,
            mpsc::RecvTimeoutError::Disconnected,
            path,
            &fatal,
        );
        assert!(
            gate.refusal_before_asking().is_none(),
            "a disconnected reply must leave the gate open"
        );
        assert!(
            matches!(revocations.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "ending the live operation over an ordinary teardown would turn it into a failure"
        );

        // The timeout is the transition: it shuts the gate, and it is the one
        // failure that wakes the coordinator.
        let wedged = refuse_input_open_after_wait_failure(
            &mut gate,
            mpsc::RecvTimeoutError::Timeout,
            path,
            &fatal,
        );
        assert_ne!(
            wedged, gone,
            "a wedged thread and an absent one must not be indistinguishable to libinput"
        );
        assert_eq!(
            gate.refusal_before_asking(),
            Some(wedged),
            "the errno a later open is refused with before paying the deadline must be the one \
             this failure produced after paying it"
        );
        assert!(
            matches!(
                revocations.try_recv(),
                Ok(LiveRevocation::SessionUnresponsive)
            ),
            "the transition must publish the notification that ends the live operation — the one \
             step of this path no test build used to compile"
        );

        // A signal keyed on "the gate is shut" rather than on the transition
        // would fire here, on a coordinator already torn down.
        let again = refuse_input_open_after_wait_failure(
            &mut gate,
            mpsc::RecvTimeoutError::Timeout,
            path,
            &fatal,
        );
        assert_eq!(again, wedged);
        assert!(
            matches!(revocations.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the coordinator is told once; it has already been woken and is tearing down"
        );
    }

    #[test]
    fn an_unanswered_close_uses_the_same_fatal_transition_exactly_once() {
        let (fatal, revocations) = mpsc::channel();
        let mut gate = InputOpenGate::default();

        let disconnected = record_input_close_wait_failure(
            &mut gate,
            mpsc::RecvTimeoutError::Disconnected,
            &fatal,
        );
        assert!(!disconnected.newly_shut);
        assert!(matches!(
            revocations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let timed_out =
            record_input_close_wait_failure(&mut gate, mpsc::RecvTimeoutError::Timeout, &fatal);
        assert!(timed_out.newly_shut);
        assert_eq!(
            revocations.recv().expect("the timeout publishes a fatal"),
            LiveRevocation::SessionUnresponsive
        );
        assert_eq!(gate.refusal_before_asking(), Some(timed_out.errno));

        let again =
            record_input_close_wait_failure(&mut gate, mpsc::RecvTimeoutError::Timeout, &fatal);
        assert!(!again.newly_shut);
        assert!(matches!(
            revocations.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn a_detached_teardown_is_reported_as_a_failure() {
        assert!(unresponsive_is_not_success(SessionTeardown::Graceful).is_ok());
        let error = unresponsive_is_not_success(SessionTeardown::Detach)
            .expect_err("a leaked session thread is not a clean exit");
        assert!(
            error.to_string().contains("stopped answering"),
            "the failure must name why the process is exiting, not just that it did: {error}"
        );
    }

    #[test]
    fn an_unresponsive_session_detaches_and_fails_the_live_operation() {
        let mut platform = FakeLiveActPlatform::revoking_with(SessionTeardown::Detach);
        let error = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("a session thread that stopped answering is not a clean exit");
        assert!(error.to_string().contains("stopped answering"), "{error}");
        assert_eq!(
            platform.closed_with,
            Some(SessionTeardown::Detach),
            "the wait's answer must be the one that reaches the teardown funnel"
        );
        // Everything else still runs, and in the same order: detaching is about
        // how the session comes down, not about skipping the funnel.
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "discard-verification",
                "start-protocol",
                "decide-adapter",
                "start-adapter",
                "wait-revocation",
                "shutdown-adapter",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn a_startup_fatal_skips_the_adapter_and_enters_the_same_teardown_funnel() {
        let mut platform =
            FakeLiveActPlatform::ending_before_adapter_with(LiveRevocation::SessionUnresponsive);
        let error = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("a fatal observed during protocol startup is not a clean exit");
        assert!(error.to_string().contains("stopped answering"), "{error}");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Detach));
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "discard-verification",
                "start-protocol",
                "decide-adapter",
                "stop-protocol",
                "close-session",
            ],
            "the render adapter and blocking revocation wait are both skipped"
        );
    }

    #[test]
    fn an_ordinary_revocation_closes_the_session_gracefully() {
        let mut platform = FakeLiveActPlatform::revoking_with(SessionTeardown::Graceful);
        operate_verified_with(&mut platform, grant_for_act_test()).expect("fake live act succeeds");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Graceful));
    }

    #[test]
    fn a_failure_before_the_revocation_wait_closes_the_session_gracefully() {
        // There is no reason to detach: nothing has waited on the session
        // thread, so it has not had a chance to stop answering.
        let mut platform = FakeLiveActPlatform::new(Some("select-target"));
        operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("the injected failure propagates");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Graceful));
    }

    #[test]
    fn live_act_orders_authority_transfer_revocation_and_teardown() {
        let mut platform = FakeLiveActPlatform::new(None);
        operate_verified_with(&mut platform, grant_for_act_test()).expect("fake live act succeeds");
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "discard-verification",
                "start-protocol",
                "decide-adapter",
                "start-adapter",
                "wait-revocation",
                "shutdown-adapter",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn external_session_pause_enters_the_pause_coordinator_without_stopping_the_pump() {
        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut mailbox = SupervisorMailbox::new(
            [started_reply(), ready_reply()],
            [
                None,
                None,
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
            ],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        let end = supervise_active_live_operation(&mut mailbox, &mut pump, now)
            .expect("external pause is handed to the pause coordinator");
        let ActiveLiveOperationEnd::PauseRequested {
            generation,
            acknowledgement,
            outstanding_command,
        } = end
        else {
            panic!("external pause ended active supervision as {end:?}");
        };
        assert_eq!(generation, 2);
        assert_eq!(outstanding_command, None);
        assert!(matches!(
            acknowledged.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
        assert_eq!(pump.commands, ["registration"]);
        assert_eq!(pump.stops, 0, "the persistent pump must remain resumable");
    }

    #[test]
    fn external_pause_cleanup_tolerates_revoked_input_close_before_acknowledgement() {
        enum FakeNotifierCommand {
            AuthorityRevoked(mpsc::SyncSender<()>),
            CloseInput(mpsc::SyncSender<Result<(), i32>>),
            Acknowledge,
        }

        let (commands, command_source) = mpsc::channel();
        let (acknowledged, acknowledgement_observed) = mpsc::channel();
        let notifier = std::thread::spawn(move || {
            let mut authority_revoked = false;
            while let Ok(command) = command_source.recv() {
                match command {
                    FakeNotifierCommand::AuthorityRevoked(reply) => {
                        authority_revoked = true;
                        let _ = reply.send(());
                    }
                    FakeNotifierCommand::CloseInput(reply) => {
                        assert!(
                            authority_revoked,
                            "the backend revokes authority before publishing PauseRequested"
                        );
                        let _ = reply.send(Err(libc::ENODEV));
                    }
                    FakeNotifierCommand::Acknowledge => {
                        let _ = acknowledged.send("protocol-acknowledged");
                        break;
                    }
                }
            }
        });
        let acknowledge_commands = commands.clone();
        let acknowledgement = ExternalPauseAcknowledgement::new(move || {
            acknowledge_commands
                .send(FakeNotifierCommand::Acknowledge)
                .is_ok()
        });

        let (revoked, revocation_observed) = mpsc::sync_channel(1);
        commands
            .send(FakeNotifierCommand::AuthorityRevoked(revoked))
            .expect("the fake backend accepts authority revocation");
        revocation_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("device authority is revoked before cleanup begins");

        let (closed, close_result) = mpsc::sync_channel(1);
        commands
            .send(FakeNotifierCommand::CloseInput(closed))
            .expect("close command is accepted while pause is pending");
        let errno = close_result
            .recv_timeout(Duration::from_secs(1))
            .expect("CloseInput answers before pause acknowledgement")
            .expect_err("the already-revoked fd reports ENODEV");
        assert!(already_revoked_device_close_is_complete(true, Some(errno)));
        assert!(matches!(
            acknowledgement_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(acknowledgement.acknowledge());
        assert_eq!(
            acknowledgement_observed
                .recv_timeout(Duration::from_secs(1))
                .expect("protocol acknowledgement follows revoked-device cleanup"),
            "protocol-acknowledged"
        );
        assert!(!already_revoked_device_close_is_complete(
            false,
            Some(libc::ENODEV)
        ));
        assert!(!already_revoked_device_close_is_complete(
            true,
            Some(libc::EIO)
        ));
        notifier.join().expect("fake notifier joins");
    }

    #[test]
    fn racing_pause_is_collected_without_losing_the_outstanding_pump_reply() {
        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut inner = SupervisorMailbox::new(
            [
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
                updated_reply(Vec::new()),
            ],
            [None],
        );
        let mut collected = None;
        let mut mailbox = PauseCollectingMailbox::new(&mut inner, &mut collected);
        let mut now = || Duration::ZERO;

        assert!(matches!(
            wait_for_transition_reply(
                &mut mailbox,
                Duration::from_secs(1),
                &mut now,
                "racing pause test",
            )
            .expect("the outstanding reply survives pause collection"),
            PumpReply::Updated(Ok(events)) if events.is_empty()
        ));
        let pause = collected.expect("the first external cause is retained");
        assert_eq!(pause.generation, 2);
        assert!(matches!(
            acknowledged.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(pause.acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
    }

    #[test]
    fn chord_after_external_pause_takeover_does_not_terminate_the_resumable_pause() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        authority
            .begin_self_switch(2)
            .expect("self-switch preparation begins");
        assert_eq!(
            authority
                .request_pause()
                .expect("external pause takes over"),
            LivePauseRequestDisposition::External { generation: 2 }
        );

        let (acknowledgement, acknowledged) = observed_pause_acknowledgement();
        let mut inner = SupervisorMailbox::new(
            [
                Some(LiveCoordinatorEvent::PauseRequested {
                    generation: 2,
                    acknowledgement,
                }),
                Some(LiveCoordinatorEvent::VtSwitchRequested(5)),
                pump_reply(PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(()),
                }),
            ],
            [None],
        );
        let mut collected = None;
        {
            let mut mailbox = PauseCollectingMailbox::new(&mut inner, &mut collected);
            let mut now = || Duration::ZERO;
            assert!(matches!(
                wait_for_transition_reply(
                    &mut mailbox,
                    Duration::from_secs(1),
                    &mut now,
                    "self-switch render suspend",
                )
                .expect("the takeover discards a later chord instead of making it terminal"),
                PumpReply::TransitionBegun {
                    generation: 2,
                    result: Ok(())
                }
            ));
        }

        let pause = collected.expect("the external pause owns the generation");
        assert_eq!(pause.generation, 2);
        assert_eq!(
            authority.complete_pause(true),
            Some(LivePauseCompletion {
                generation: 2,
                cause: LivePauseCause::External,
                resumable: true,
                activate_pending: false,
            })
        );
        assert!(pause.acknowledgement.acknowledge());
        assert_eq!(acknowledged.recv().expect("ack observed"), "acknowledged");
        assert_eq!(
            authority
                .begin_resume()
                .expect("the takeover remains resumable"),
            3
        );
    }

    #[test]
    fn self_switch_pauses_without_stopping_the_production_pump() {
        let mut mailbox = SupervisorMailbox::new(
            [started_reply(), ready_reply()],
            [None, None, Some(LiveCoordinatorEvent::VtSwitchRequested(4))],
        );
        let mut pump = SupervisorPump::at_60_hz();
        let now = mailbox.now();

        assert_eq!(
            supervise_active_live_operation(&mut mailbox, &mut pump, now)
                .expect("self-switch is handed to the pause coordinator"),
            ActiveLiveOperationEnd::VtSwitchRequested {
                vt: 4,
                outstanding_command: None,
            }
        );
        assert_eq!(pump.stops, 0, "the persistent pump must remain resumable");
    }

    #[test]
    fn vt_switch_reaches_the_session_only_after_adapter_quiescence() {
        let (pump, barrier) =
            super::super::render::LiveRenderPump::blocked_for_test(Duration::from_secs(1));
        let (switch_vt, switch_observed) = mpsc::sync_channel(1);
        let coordinator = std::thread::spawn(move || {
            let mut platform = RealPumpSwitchOrderPlatform { switch_vt };
            finish_live_operation(
                &mut platform,
                Some(pump),
                Some(()),
                Ok(SessionTeardown::Graceful),
            )
        });

        barrier.wait_for_stop();
        barrier.release_completion_and_wait();
        barrier.assert_thread_still_running();
        assert!(matches!(
            switch_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.release_thread_exit();
        barrier.wait_for_joined_transition();
        assert!(matches!(
            switch_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.resume_transition();
        switch_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("the session seam receives SwitchVt after the real pump joins");
        coordinator
            .join()
            .expect("coordinator test thread joins")
            .expect("joined pump permits graceful VT-switch teardown");
    }

    #[test]
    fn failed_suspend_with_a_wedged_pump_detaches_before_vt_switch() {
        let (pump, barrier) =
            super::super::render::LiveRenderPump::blocked_for_test(Duration::from_millis(20));
        let (switch_vt, switch_observed) = mpsc::sync_channel(1);
        let coordinator = std::thread::spawn(move || {
            let mut platform = RealPumpSwitchOrderPlatform { switch_vt };
            finish_live_operation(
                &mut platform,
                Some(pump),
                Some(()),
                Err(KmsLiveError::Setup(
                    "injected render suspend failure".into(),
                )),
            )
        });

        barrier.wait_for_stop();
        barrier.assert_thread_still_running();
        assert!(matches!(
            switch_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.wait_for_detached_transition();
        barrier.assert_thread_still_running();
        assert!(matches!(
            switch_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.resume_transition();
        switch_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("the session seam receives SwitchVt after the real pump detaches");
        let error = coordinator
            .join()
            .expect("coordinator test thread joins")
            .expect_err("a detached pump remains an honest nonzero ending");
        assert!(
            error
                .to_string()
                .contains("injected render suspend failure")
        );
        assert!(error.to_string().contains("did not quiesce"));
        barrier.release_all_and_wait();
    }

    #[test]
    fn wedged_external_pause_detaches_terminally_then_releases_the_seat() {
        let (pump, barrier) =
            super::super::render::LiveRenderPump::blocked_for_test(Duration::from_millis(20));
        let (acknowledge, acknowledgement_observed) = mpsc::sync_channel(1);
        let coordinator = std::thread::spawn(move || {
            let mut platform = RealPumpSwitchOrderPlatform {
                switch_vt: acknowledge,
            };
            finish_live_operation(
                &mut platform,
                Some(pump),
                Some(()),
                Err(KmsLiveError::Setup(
                    "injected external authority-revoked suspend failure".into(),
                )),
            )
        });

        barrier.wait_for_stop();
        assert!(matches!(
            acknowledgement_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.wait_for_detached_transition();
        barrier.assert_thread_still_running();
        assert!(matches!(
            acknowledgement_observed.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        barrier.resume_transition();
        acknowledgement_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("seat acknowledgement follows bounded renderer detach");
        let error = coordinator
            .join()
            .expect("coordinator test thread joins")
            .expect_err("renderer detach keeps the external pause terminal");
        assert!(
            error
                .to_string()
                .contains("authority-revoked suspend failure")
        );
        assert!(error.to_string().contains("did not quiesce"));
        barrier.release_all_and_wait();
    }

    #[test]
    fn switch_then_session_pause_is_one_graceful_teardown() {
        let mut platform = FakeLiveActPlatform::switching_vt(3);
        platform.pause_after_vt_switch = true;
        operate_verified_with(&mut platform, grant_for_act_test())
            .expect("the requested switch's pause confirmation is ordinary");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Graceful));
        assert_eq!(
            platform
                .events
                .iter()
                .filter(|event| **event == "switch-vt")
                .count(),
            1
        );
        assert_eq!(
            platform
                .events
                .iter()
                .filter(|event| **event == "close-session")
                .count(),
            1
        );
    }

    #[test]
    fn vt_switch_reply_timeout_does_not_block_the_close_funnel() {
        assert_eq!(VT_SWITCH_REPLY_TIMEOUT, Duration::from_secs(1));
        assert_eq!(
            vt_switch_ask_after_wait(Err(mpsc::RecvTimeoutError::Timeout)),
            VtSwitchAsk::TimedOut
        );
        let mut platform = FakeLiveActPlatform::switching_vt(3);
        operate_verified_with(&mut platform, grant_for_act_test())
            .expect("an unanswered advisory VT request still exits cleanly");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Graceful));
        assert_eq!(
            &platform.events[platform.events.len() - 3..],
            ["switch-vt", "stop-protocol", "close-session"]
        );
    }

    #[test]
    fn live_act_dup_failure_still_enters_session_cleanup() {
        let mut platform = FakeLiveActPlatform::new(Some("duplicate-lease"));
        let _ = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("injected dup failure is returned");
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "close-session",
            ]
        );
    }

    #[test]
    fn live_act_authority_open_failure_enters_explicit_session_cleanup() {
        let mut platform = FakeLiveActPlatform::new(Some("authority-open"));
        let _ = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("injected authority open failure is returned");
        assert_eq!(platform.events, ["authority-open", "close-session"]);
    }

    #[test]
    fn terminal_event_before_authority_open_never_opens_drm() {
        let mut platform = FakeLiveActPlatform::new(Some("before-authority-open"));
        let _ = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("pre-authority arbitration terminates the operation");
        assert_eq!(platform.events, ["before-authority-open", "close-session"]);
    }

    #[test]
    fn terminal_event_before_protocol_start_never_starts_protocol() {
        let mut platform = FakeLiveActPlatform::new(Some("before-protocol-start"));
        let _ = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("pre-protocol arbitration terminates the operation");
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "discard-verification",
                "before-protocol-start",
                "close-session",
            ]
        );
    }

    #[test]
    fn signal_queued_during_protocol_start_refuses_the_render_adapter() {
        let signal = LiveSignal::Terminate;
        assert_eq!(
            adapter_start_after_events(&[LiveCoordinatorEvent::Signal(signal)], None),
            AdapterStartDecision::EndSignal(signal)
        );
        assert_eq!(
            adapter_start_after_events(&[], Some(signal)),
            AdapterStartDecision::EndSignal(signal),
            "the durable latch covers a signal whose mailbox send has not arrived"
        );
        let mut platform = FakeLiveActPlatform::signalling_before_adapter(signal);

        let error = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("queued signal terminates before adapter startup");

        assert!(matches!(error, KmsLiveError::Signal(LiveSignal::Terminate)));
        assert_eq!(error.exit_code(), Some(143));
        assert!(!platform.events.contains(&"start-adapter"));
        assert_eq!(
            platform.events,
            [
                "authority-open",
                "select-target",
                "duplicate-lease",
                "discard-verification",
                "start-protocol",
                "decide-adapter",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn external_pause_after_open_closes_drm_before_acknowledging_and_closing_session() {
        let mut platform = FakeLiveActPlatform::pausing_before_protocol();

        operate_verified_with(&mut platform, grant_for_act_test())
            .expect("a startup external pause is an ordinary authority end");

        assert!(!platform.events.contains(&"start-protocol"));
        assert_eq!(
            &platform.events[platform.events.len() - 5..],
            [
                "before-protocol-start",
                "shutdown-unstarted-pump",
                "close-original",
                "acknowledge-external-pause",
                "close-session",
            ]
        );
    }

    #[test]
    fn external_pause_before_adapter_reconciles_input_and_closes_drm_before_acknowledging() {
        let mut platform = FakeLiveActPlatform::pausing_before_adapter();

        operate_verified_with(&mut platform, grant_for_act_test())
            .expect("a pre-adapter external pause is an ordinary authority end");

        assert!(!platform.events.contains(&"start-adapter"));
        assert_eq!(
            &platform.events[platform.events.len() - 6..],
            [
                "shutdown-unstarted-pump",
                "reconcile-input",
                "close-original",
                "acknowledge-external-pause",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn vt_switch_queued_during_protocol_start_skips_the_render_adapter() {
        assert_eq!(
            adapter_start_after_events(&[LiveCoordinatorEvent::VtSwitchRequested(5)], None),
            AdapterStartDecision::EndVtSwitch(5)
        );
        assert_eq!(
            adapter_start_after_events(
                &[
                    LiveCoordinatorEvent::VtSwitchRequested(5),
                    LiveCoordinatorEvent::Revocation(LiveRevocation::SessionPause),
                ],
                None,
            ),
            AdapterStartDecision::EndVtSwitch(5),
            "the operator request is the first cause"
        );
        assert_eq!(
            adapter_start_after_events(
                &[
                    LiveCoordinatorEvent::Revocation(LiveRevocation::SessionPause),
                    LiveCoordinatorEvent::VtSwitchRequested(5),
                ],
                None,
            ),
            AdapterStartDecision::EndAuthority(LiveRevocation::SessionPause),
            "an independently published pause that arrived first remains the cause"
        );
        let mut platform = FakeLiveActPlatform::switching_before_adapter(5);

        operate_verified_with(&mut platform, grant_for_act_test())
            .expect("a pre-adapter VT switch is graceful");

        assert!(!platform.events.contains(&"start-adapter"));
        assert_eq!(
            &platform.events[platform.events.len() - 5..],
            [
                "decide-adapter",
                "shutdown-unstarted-pump",
                "switch-vt",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn live_act_revocation_failure_uses_the_same_ordered_teardown_funnel() {
        let mut platform = FakeLiveActPlatform::new(Some("wait-revocation"));
        let _ = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("injected revocation failure is returned");
        assert_eq!(platform.closed_with, Some(SessionTeardown::Graceful));
        assert_eq!(
            &platform.events[platform.events.len() - 4..],
            [
                "wait-revocation",
                "shutdown-adapter",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn live_act_preserves_revocation_and_shutdown_failures_together() {
        let mut platform =
            FakeLiveActPlatform::failing_at(&["wait-revocation", "shutdown-adapter"]);
        let error = operate_verified_with(&mut platform, grant_for_act_test())
            .expect_err("both injected failures are returned");
        let message = error.to_string();
        assert!(message.contains("injected wait-revocation failure"));
        assert!(message.contains("injected shutdown-adapter failure"));
        assert_eq!(
            &platform.events[platform.events.len() - 4..],
            [
                "wait-revocation",
                "shutdown-adapter",
                "stop-protocol",
                "close-session",
            ]
        );
    }

    #[test]
    fn every_post_open_setup_failure_reaches_the_single_teardown_funnel() {
        let cases: &[(&str, &[&str])] = &[
            ("select-target", &["select-target", "close-session"]),
            ("start-protocol", &["start-protocol", "close-session"]),
            (
                "start-adapter",
                &["start-adapter", "stop-protocol", "close-session"],
            ),
            (
                "shutdown-adapter",
                &["shutdown-adapter", "stop-protocol", "close-session"],
            ),
        ];
        for (fail_at, expected_suffix) in cases {
            let mut platform = FakeLiveActPlatform::new(Some(fail_at));
            let _ = operate_verified_with(&mut platform, grant_for_act_test())
                .expect_err("the selected fake stage fails");
            assert_eq!(
                &platform.events[platform.events.len() - expected_suffix.len()..],
                *expected_suffix,
                "failure at {fail_at} must retain the ordered cleanup suffix"
            );
        }
    }

    #[test]
    fn session_device_close_consumes_the_retained_original_exactly_once() {
        let mut original = Some(harmless_fd());
        let closes = Cell::new(0);
        close_retained_session_device(&mut original, |fd| {
            closes.set(closes.get() + 1);
            drop(fd);
            Ok::<(), ()>(())
        })
        .expect("fake session close succeeds");
        close_retained_session_device(&mut original, |_fd| {
            closes.set(closes.get() + 1);
            Ok::<(), ()>(())
        })
        .expect("an empty owner needs no second close");
        assert!(original.is_none());
        assert_eq!(closes.get(), 1);
    }

    #[test]
    fn pre_open_revocation_refuses_before_the_open_closure_runs() {
        let mut authority = LiveSessionAuthority::initial();
        assert!(latch_live_revocation(
            &mut authority,
            LiveRevocation::SessionPause
        ));
        let mut original = None;
        let opened = Cell::new(false);
        let error = open_authorised_session_device(
            &mut authority,
            true,
            &mut original,
            || -> Result<OwnedFd, std::io::Error> {
                opened.set(true);
                Ok(harmless_fd())
            },
        )
        .expect_err("a delivered pre-open revocation must refuse authority");
        assert_eq!(error, KmsLiveRefusal::RevokedBeforeAuthorityOpen);
        assert!(!opened.get());
        assert!(original.is_none());
    }

    #[test]
    fn calloop_batch_finishes_every_ready_source_before_deferred_open() {
        let mut event_loop = EventLoop::<Vec<&'static str>>::try_new().expect("fake calloop");
        let (open_sender, open_source) = channel::channel();
        let (revocation_sender, revocation_source) = channel::channel();
        event_loop
            .handle()
            .insert_source(open_source, |event, (), events| {
                if matches!(event, channel::Event::Msg(())) {
                    events.push("open-command");
                }
            })
            .expect("fake open source");
        event_loop
            .handle()
            .insert_source(revocation_source, |event, (), events| {
                if matches!(event, channel::Event::Msg(())) {
                    events.push("revocation");
                }
            })
            .expect("fake revocation source");
        open_sender.send(()).expect("queue fake open");
        revocation_sender.send(()).expect("queue fake revocation");

        let mut events = Vec::new();
        event_loop
            .dispatch(Some(std::time::Duration::ZERO), &mut events)
            .expect("dispatch complete readiness batch");
        events.push("authority-open");

        assert_eq!(events.last(), Some(&"authority-open"));
        assert!(events[..events.len() - 1].contains(&"open-command"));
        assert!(events[..events.len() - 1].contains(&"revocation"));
    }

    #[test]
    fn a_second_queued_open_is_refused_without_displacing_the_first() {
        let mut pending_open = None;
        queue_live_session_open(&mut pending_open, "first").expect("the first open queues");
        assert_eq!(
            queue_live_session_open(&mut pending_open, "second")
                .expect_err("a second open must be refused, not queued"),
            "second"
        );
        assert_eq!(
            pending_open,
            Some("first"),
            "the refused request must not displace the pending one, whose reply channel is the \
             only way its caller learns the outcome"
        );
    }

    #[test]
    fn session_round_dispatches_revocation_before_pending_authority_open() {
        struct OfflineSessionRound {
            authority: LiveSessionAuthority,
            original: Option<OwnedFd>,
            pending_open: Option<()>,
            open_result: Option<Result<OwnedFd, KmsLiveError>>,
            events: Vec<&'static str>,
        }

        let attempted_drm_open = Cell::new(false);
        let mut state = OfflineSessionRound {
            authority: LiveSessionAuthority::initial(),
            original: None,
            pending_open: None,
            open_result: None,
            events: Vec::new(),
        };
        queue_live_session_open(&mut state.pending_open, ())
            .expect("the command queues an open without performing it");

        dispatch_live_session_round(
            &mut state,
            |state| {
                state.events.push("dispatch-start");
                assert!(latch_live_revocation(
                    &mut state.authority,
                    LiveRevocation::SessionPause,
                ));
                state.events.push("dispatch-complete");
                Ok::<(), std::convert::Infallible>(())
            },
            |_state| false,
            |state| {
                state.events.push("pending-open-step");
                state
                    .pending_open
                    .take()
                    .expect("the queued open remains pending until this step");
                state.open_result = Some(open_authorised_session_device(
                    &mut state.authority,
                    true,
                    &mut state.original,
                    || -> Result<OwnedFd, std::io::Error> {
                        attempted_drm_open.set(true);
                        Ok(harmless_fd())
                    },
                ));
            },
        )
        .expect("offline session round dispatch succeeds");

        assert_eq!(
            state.events,
            ["dispatch-start", "dispatch-complete", "pending-open-step"]
        );
        let error = state
            .open_result
            .expect("the pending open step ran after dispatch")
            .expect_err("the dispatched revocation must refuse authority");
        assert_eq!(error, KmsLiveRefusal::RevokedBeforeAuthorityOpen);
        assert!(!attempted_drm_open.get());
        assert!(state.original.is_none());
    }

    #[test]
    fn inactive_session_refuses_before_the_open_closure_runs() {
        let mut authority = LiveSessionAuthority::initial();
        let mut original = None;
        let opened = Cell::new(false);
        let error = open_authorised_session_device(
            &mut authority,
            false,
            &mut original,
            || -> Result<OwnedFd, std::io::Error> {
                opened.set(true);
                Ok(harmless_fd())
            },
        )
        .expect_err("an inactive libseat session must refuse authority");
        assert_eq!(error, KmsLiveRefusal::SessionInactiveBeforeAuthorityOpen);
        assert!(!opened.get());
        assert!(original.is_none());
        assert_eq!(authority, LiveSessionAuthority::initial());
    }

    #[test]
    fn successful_open_atomically_retains_original_and_returns_a_distinct_dup() {
        let mut authority = LiveSessionAuthority::initial();
        let mut original = None;
        let verification =
            open_authorised_session_device(&mut authority, true, &mut original, || {
                Ok::<_, std::io::Error>(harmless_fd())
            })
            .expect("fake active session opens");
        assert_eq!(authority, LiveSessionAuthority::Active { generation: 1 });
        assert_ne!(
            verification.as_raw_fd(),
            original.as_ref().expect("original retained").as_raw_fd()
        );
    }

    #[test]
    fn only_the_first_post_open_revocation_requests_delivery() {
        let mut authority = LiveSessionAuthority::Active { generation: 1 };
        assert!(latch_live_revocation(
            &mut authority,
            LiveRevocation::SessionPause
        ));
        assert!(!latch_live_revocation(
            &mut authority,
            LiveRevocation::TargetHotplug
        ));
        assert_eq!(
            authority,
            LiveSessionAuthority::Paused {
                generation: 1,
                revocation: LiveRevocation::SessionPause,
            }
        );
    }

    fn injected_grant(
        platform: Rc<dyn GrantPlatform>,
        confirmation: &mut dyn ConfirmationIo,
    ) -> Result<KmsLiveGrant, KmsLiveRefusal> {
        authorise_observed(
            parse_request(&argv()).expect("request"),
            build(),
            harmless_fd(),
            platform,
            confirmation,
        )
    }

    fn ordinary_grant(platform: Rc<FakePlatform>) -> Result<KmsLiveGrant, KmsLiveRefusal> {
        injected_grant(platform, &mut FakeConfirmation::typed(CODE))
    }

    fn grant_refusal(result: Result<KmsLiveGrant, KmsLiveRefusal>) -> KmsLiveRefusal {
        match result {
            Ok(_) => panic!("interlock unexpectedly granted authority"),
            Err(error) => error,
        }
    }

    #[test]
    fn confirmation_source_failure_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.after_prompt = Some(Err(KmsLiveRefusal::ConfirmationReadFailed));
        let error = match injected_grant(platform, &mut confirmation) {
            Ok(_) => panic!("confirmation read must fail"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::ConfirmationReadFailed);
    }

    #[test]
    fn queued_pre_prompt_confirmation_is_flushed_and_refused() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut confirmation = FakeConfirmation {
            queued_before_prompt: Some(CODE.into()),
            flush_result: 0,
            after_prompt: None,
            prompt_displayed: false,
            displayed_intent: None,
            displayed_code: None,
            events: Vec::new(),
        };
        let error = match injected_grant(platform, &mut confirmation) {
            Ok(_) => panic!("pre-prompt input must not authorise"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::ConfirmationReadFailed);
        assert!(confirmation.prompt_displayed);
        assert_eq!(confirmation.events, ["flush", "prompt", "read"]);
    }

    #[test]
    fn tty_input_flush_failure_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.flush_result = -1;
        let error = match injected_grant(platform, &mut confirmation) {
            Ok(_) => panic!("failed input flush must refuse"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::TtyInputFlushFailed);
        assert!(!confirmation.prompt_displayed);
        assert_eq!(confirmation.events, ["flush"]);
    }

    #[test]
    fn unavailable_legacy_tiocsti_state_refuses_before_confirmation() {
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform
            .legacy_tiocsti
            .set(Err(KmsLiveRefusal::TtyLegacyInjectionStateUnavailable));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::TtyLegacyInjectionStateUnavailable);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn enabled_legacy_tiocsti_refuses_before_confirmation() {
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform.legacy_tiocsti.set(Ok(true));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::TtyLegacyInjectionEnabled);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn nonce_failure_occurs_after_flush_and_before_prompt() {
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform
            .nonce
            .set(Err(KmsLiveRefusal::ConfirmationNonceUnavailable));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::ConfirmationNonceUnavailable);
        assert_eq!(confirmation.events, ["flush"]);
        assert!(!confirmation.prompt_displayed);
    }

    #[test]
    fn confirmation_is_bound_to_the_post_flush_nonce() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        platform.nonce.set(Ok([0x6b; CONFIRMATION_NONCE_BYTES]));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::ConfirmationMismatch);
        assert_eq!(confirmation.displayed_intent.as_deref(), Some(INTENT));
        assert_eq!(confirmation.displayed_code.as_deref(), Some("6b6b6b6b"));
    }

    #[test]
    fn device_incarnation_open_failure_refuses_before_prompt() {
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform
            .incarnation_hold_error
            .set(Some(KmsLiveRefusal::DeviceIncarnationOpenFailed));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::DeviceIncarnationOpenFailed);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn device_incarnation_initial_read_failure_refuses_before_prompt() {
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform
            .incarnation_hold_error
            .set(Some(KmsLiveRefusal::DeviceIncarnationReadFailed));
        let mut confirmation = FakeConfirmation::typed(CODE);
        let error = grant_refusal(injected_grant(platform, &mut confirmation));
        assert_eq!(error, KmsLiveRefusal::DeviceIncarnationReadFailed);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn post_confirmation_revalidation_rejects_a_different_but_active_vt() {
        let mut changed = vt();
        changed.tty_minor = 1;
        changed.active_vt = Some(1);
        let platform = Rc::new(FakePlatform::new([vt(), changed]));
        let error = match ordinary_grant(platform) {
            Ok(_) => panic!("post-confirmation VT change must refuse"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::VtChangedSinceAuthorisation);
    }

    fn unattended_grant(platform: Rc<FakePlatform>) -> Result<KmsLiveGrant, KmsLiveRefusal> {
        // The default (no `--kms-confirm`) authorisation path. The confirmation
        // sink still has to flush pending tty input, but the prompt/read must
        // never fire — so `after_prompt` is armed with a read failure that would
        // surface as ConfirmationReadFailed if the unattended path wrongly read.
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.after_prompt = Some(Err(KmsLiveRefusal::ConfirmationReadFailed));
        authorise_observed(
            parse_request(&argv_unattended()).expect("unattended request"),
            build(),
            harmless_fd(),
            platform,
            &mut confirmation,
        )
    }

    #[test]
    fn unattended_default_grants_without_prompt_or_read() {
        // Default takeover: no operator answers a nonce, so the interlock must
        // flush tty input and then proceed — no prompt, no read, and the nonce
        // source is never consulted. This is the "remove the human from the
        // loop" path an agent drives.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        platform
            .nonce
            .set(Err(KmsLiveRefusal::ConfirmationNonceUnavailable));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.after_prompt = Some(Err(KmsLiveRefusal::ConfirmationReadFailed));
        authorise_observed(
            parse_request(&argv_unattended()).expect("unattended request"),
            build(),
            harmless_fd(),
            Rc::clone(&platform) as Rc<dyn GrantPlatform>,
            &mut confirmation,
        )
        .expect("an unattended takeover grants without a typed confirmation");
        assert_eq!(confirmation.events, ["flush"]);
        assert!(!confirmation.prompt_displayed);
        assert!(confirmation.displayed_code.is_none());
    }

    #[test]
    fn unattended_still_requires_a_successful_tty_input_flush() {
        // Dropping the typed challenge must not drop the pre-takeover input
        // flush — a failed flush still refuses before authority changes hands.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.flush_result = -1;
        let error = grant_refusal(authorise_observed(
            parse_request(&argv_unattended()).expect("unattended request"),
            build(),
            harmless_fd(),
            Rc::clone(&platform) as Rc<dyn GrantPlatform>,
            &mut confirmation,
        ));
        assert_eq!(error, KmsLiveRefusal::TtyInputFlushFailed);
        assert_eq!(confirmation.events, ["flush"]);
    }

    #[test]
    fn unattended_still_refuses_a_vt_change_since_authorisation() {
        // The typed nonce was never the device/VT binding; the post-observation
        // continuity checks are. They stay unconditional, so an unattended
        // takeover still refuses a VT that drifted between observations.
        let mut changed = vt();
        changed.tty_minor = 1;
        changed.active_vt = Some(1);
        let platform = Rc::new(FakePlatform::new([vt(), changed]));
        let error = grant_refusal(unattended_grant(platform));
        assert_eq!(error, KmsLiveRefusal::VtChangedSinceAuthorisation);
    }

    #[test]
    fn unattended_still_refuses_a_device_stable_identity_change() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut replacement = device();
        replacement.stable_device_path = Some("/sys/devices/pci0000:00/0000:00:03.0".into());
        *platform.devices.borrow_mut() = [device(), replacement].into();
        let error = grant_refusal(unattended_grant(platform));
        assert_eq!(error, KmsLiveRefusal::DeviceStableIdentityChanged);
    }

    #[test]
    fn unattended_still_refuses_a_canonical_identity_change() {
        // The old typed token carried the canonical path, so a stable-path
        // re-resolution to a different card used to mismatch the confirmation
        // incidentally. The continuity check now owns that binding and runs
        // unconditionally — an unattended takeover must still refuse the drift
        // before the authority-changing open.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut replacement = device();
        replacement.canonical_path = Some("/dev/dri/card1".into());
        replacement.node_rdev = libc::makedev(226, 1);
        replacement.udev_rdev = Some(libc::makedev(226, 1));
        *platform.devices.borrow_mut() = [device(), replacement].into();
        let error = grant_refusal(unattended_grant(Rc::clone(&platform)));
        assert_eq!(error, KmsLiveRefusal::DeviceCanonicalIdentityChanged);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn unattended_still_refuses_a_vanished_connector() {
        // Connector re-presence across the authorisation window is a binding
        // guard, not ceremony — an unattended takeover still refuses a
        // connector that disappeared between the observations.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut refreshed = device();
        refreshed.connectors.clear();
        *platform.devices.borrow_mut() = [device(), refreshed].into();
        let error = grant_refusal(unattended_grant(Rc::clone(&platform)));
        assert_eq!(error, KmsLiveRefusal::ConnectorNotPresent);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn unattended_still_holds_the_device_incarnation() {
        // The device-incarnation hold runs before the confirm branch and stays
        // unconditional: a failed hold refuses an unattended takeover before any
        // tty input flush, exactly as on the attended path.
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform
            .incarnation_hold_error
            .set(Some(KmsLiveRefusal::DeviceIncarnationOpenFailed));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.after_prompt = Some(Err(KmsLiveRefusal::ConfirmationReadFailed));
        let error = grant_refusal(authorise_observed(
            parse_request(&argv_unattended()).expect("unattended request"),
            build(),
            harmless_fd(),
            Rc::clone(&platform) as Rc<dyn GrantPlatform>,
            &mut confirmation,
        ));
        assert_eq!(error, KmsLiveRefusal::DeviceIncarnationOpenFailed);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn unattended_still_refuses_enabled_legacy_tiocsti() {
        // Legacy TIOCSTI defence-in-depth refuses before the confirm branch and
        // stays unconditional — an unattended takeover on a tty that permits
        // legacy injection still refuses before any flush.
        let platform = Rc::new(FakePlatform::new([vt()]));
        platform.legacy_tiocsti.set(Ok(true));
        let mut confirmation = FakeConfirmation::typed(CODE);
        confirmation.after_prompt = Some(Err(KmsLiveRefusal::ConfirmationReadFailed));
        let error = grant_refusal(authorise_observed(
            parse_request(&argv_unattended()).expect("unattended request"),
            build(),
            harmless_fd(),
            Rc::clone(&platform) as Rc<dyn GrantPlatform>,
            &mut confirmation,
        ));
        assert_eq!(error, KmsLiveRefusal::TtyLegacyInjectionEnabled);
        assert!(confirmation.events.is_empty());
    }

    #[test]
    fn destructive_boundary_revalidates_internally() {
        let mut changed = vt();
        changed.active_vt = Some(1);
        let platform = Rc::new(FakePlatform::new([vt(), vt(), changed]));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        let error = execute_verified_test(grant, Rc::clone(&platform))
            .expect_err("boundary VT change must refuse");
        assert_eq!(error, KmsLiveRefusal::TtyNotActive);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn final_vt_validation_remains_after_open_and_connector_checks() {
        let mut changed = vt();
        changed.active_vt = Some(1);
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt(), changed]));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        let error = execute_verified_test(grant, Rc::clone(&platform))
            .expect_err("final VT change must refuse");
        assert_eq!(error, KmsLiveRefusal::TtyNotActive);
        assert_eq!(platform.drm_open_count.get(), 1);
        assert_eq!(platform.incarnation_validation_count.get(), 1);
        assert_eq!(platform.connector_scan_count.get(), 1);
    }

    #[test]
    fn device_identity_is_refreshed_after_confirmation() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut replacement = device();
        replacement.stable_device_path = Some("/sys/devices/pci0000:00/0000:00:03.0".into());
        *platform.devices.borrow_mut() = [device(), replacement].into();
        let error = match ordinary_grant(platform) {
            Ok(_) => panic!("post-confirmation device replacement must refuse"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::DeviceStableIdentityChanged);
    }

    #[test]
    fn canonical_drift_under_an_unchanged_stable_path_is_refused_before_open() {
        // The old typed token carried the canonical path, so this drift used
        // to mismatch the confirmation incidentally. With the token reduced to
        // a freshness code, the continuity check owns it: a stable parent path
        // re-resolving to a different card between the observations must
        // refuse before the authority-changing open, not after it.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut replacement = device();
        replacement.canonical_path = Some("/dev/dri/card1".into());
        replacement.node_rdev = libc::makedev(226, 1);
        replacement.udev_rdev = Some(libc::makedev(226, 1));
        *platform.devices.borrow_mut() = [device(), replacement].into();
        let error = grant_refusal(ordinary_grant(Rc::clone(&platform)));
        assert_eq!(error, KmsLiveRefusal::DeviceCanonicalIdentityChanged);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn a_fresh_non_default_code_is_displayed_and_accepted() {
        // The rejection half alone cannot pin that the comparison uses the
        // fresh nonce: a comparison stuck on a stale default code would still
        // reject a mismatched line. Only a grant earned by retyping a
        // non-default, non-repeating displayed code proves display and
        // comparison read the same fresh value.
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        platform.nonce.set(Ok([0x01, 0x23, 0x45, 0x67]));
        let mut confirmation = FakeConfirmation::typed("01234567");
        injected_grant(
            Rc::clone(&platform) as Rc<dyn GrantPlatform>,
            &mut confirmation,
        )
        .expect("a correctly retyped fresh code grants");
        assert_eq!(confirmation.displayed_code.as_deref(), Some("01234567"));
    }

    #[test]
    fn connector_vanishing_during_confirmation_is_refused_before_open() {
        let platform = Rc::new(FakePlatform::new([vt(), vt()]));
        let mut refreshed = device();
        refreshed.connectors.clear();
        *platform.devices.borrow_mut() = [device(), refreshed].into();
        let error = match ordinary_grant(Rc::clone(&platform)) {
            Ok(_) => panic!("post-confirmation connector removal must refuse"),
            Err(error) => error,
        };
        assert_eq!(error, KmsLiveRefusal::ConnectorNotPresent);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn destructive_boundary_rejects_a_different_but_active_vt() {
        let mut changed = vt();
        changed.tty_minor = 1;
        changed.active_vt = Some(1);
        let platform = Rc::new(FakePlatform::new([vt(), vt(), changed]));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        let error = execute_verified_test(grant, Rc::clone(&platform))
            .expect_err("different VT must refuse");
        assert_eq!(error, KmsLiveRefusal::VtChangedSinceAuthorisation);
        assert_eq!(platform.drm_open_count.get(), 0);
    }

    #[test]
    fn dev_t_identical_stable_identity_change_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        *platform.opened_identity.borrow_mut() =
            Ok(opened_identity("/sys/devices/pci0000:00/0000:00:03.0"));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        let error = execute_verified_test(grant, Rc::clone(&platform))
            .expect_err("different stable device must refuse");
        assert_eq!(
            platform
                .opened_identity
                .borrow()
                .as_ref()
                .expect("identity")
                .rdev,
            libc::makedev(226, 0)
        );
        assert_eq!(error, KmsLiveRefusal::DeviceStableIdentityChanged);
        assert_eq!(platform.incarnation_validation_count.get(), 0);
    }

    #[test]
    fn removed_held_incarnation_refuses_before_connector_or_final_vt() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform
            .incarnation_validation
            .set(Err(KmsLiveRefusal::DeviceIncarnationGone));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        assert_eq!(
            execute_verified_test(grant, Rc::clone(&platform))
                .expect_err("removed incarnation must refuse"),
            KmsLiveRefusal::DeviceIncarnationGone
        );
        assert_eq!(platform.incarnation_validation_count.get(), 1);
        assert_eq!(platform.connector_scan_count.get(), 0);
    }

    #[test]
    fn held_incarnation_reread_failure_refuses_before_connector() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform
            .incarnation_validation
            .set(Err(KmsLiveRefusal::DeviceIncarnationReadFailed));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        assert_eq!(
            execute_verified_test(grant, Rc::clone(&platform))
                .expect_err("unreadable incarnation must refuse"),
            KmsLiveRefusal::DeviceIncarnationReadFailed
        );
        assert_eq!(platform.connector_scan_count.get(), 0);
    }

    #[test]
    fn replacement_incarnation_refuses_before_connector() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform
            .incarnation_validation
            .set(Err(KmsLiveRefusal::DeviceIncarnationChanged));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        assert_eq!(
            execute_verified_test(grant, Rc::clone(&platform))
                .expect_err("replacement incarnation must refuse"),
            KmsLiveRefusal::DeviceIncarnationChanged
        );
        assert_eq!(platform.connector_scan_count.get(), 0);
    }

    #[test]
    fn connector_vanishing_at_boundary_refuses_before_operation() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform.connector.set(Ok(None));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        let error = execute_verified_test(grant, Rc::clone(&platform))
            .expect_err("vanished connector must refuse");
        assert_eq!(error, KmsLiveRefusal::ConnectorNotPresent);
        assert_eq!(platform.connector_scan_count.get(), 1);
        assert_eq!(platform.incarnation_validation_count.get(), 1);
    }

    #[test]
    fn final_boundary_checks_are_contiguous_before_the_module_owned_act() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt(), vt()]));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("initial grant");
        platform.boundary_events.borrow_mut().clear();
        assert_eq!(
            execute_verified_test(grant, Rc::clone(&platform))
                .expect_err("offline live act remains unavailable"),
            KmsLiveRefusal::LiveBodyUnavailable
        );
        assert_eq!(
            platform.boundary_events.borrow().as_slice(),
            [
                "vt",
                "open",
                "observe-open",
                "incarnation",
                "connector",
                "vt"
            ]
        );
        assert_eq!(platform.drm_open_count.get(), 1);
        assert_eq!(platform.incarnation_validation_count.get(), 1);
        assert_eq!(platform.connector_scan_count.get(), 1);
    }

    #[test]
    fn opened_drm_node_observation_failure_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        *platform.opened_identity.borrow_mut() = Err(KmsLiveRefusal::DrmNodeObservationUnavailable);
        let grant = ordinary_grant(Rc::clone(&platform)).expect("grant");
        assert_eq!(
            execute_verified_test(grant, platform).expect_err("fstat failure"),
            KmsLiveRefusal::DrmNodeObservationUnavailable
        );
    }

    #[test]
    fn drm_node_open_failure_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform
            .drm_open_error
            .set(Some(KmsLiveRefusal::DrmNodeOpenFailed));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("grant");
        assert_eq!(
            execute_verified_test(grant, platform).expect_err("open failure"),
            KmsLiveRefusal::DrmNodeOpenFailed
        );
    }

    #[test]
    fn connector_boundary_scan_failure_has_a_sole_falsifier() {
        let platform = Rc::new(FakePlatform::new([vt(), vt(), vt()]));
        platform
            .connector
            .set(Err(KmsLiveRefusal::ConnectorBoundaryScanFailed));
        let grant = ordinary_grant(Rc::clone(&platform)).expect("grant");
        assert_eq!(
            execute_verified_test(grant, platform).expect_err("connector scan failure"),
            KmsLiveRefusal::ConnectorBoundaryScanFailed
        );
    }

    #[test]
    fn grant_is_not_clone_copy_send_or_sync() {
        static_assertions::assert_not_impl_any!(KmsLiveGrant: Clone, Copy, Send, Sync);
        static_assertions::assert_not_impl_any!(VerifiedDrmFd: Clone, Copy);
        static_assertions::assert_not_impl_any!(MasterDrmLease: Clone, Copy);
        static_assertions::assert_impl_all!(MasterDrmLease: Send);
    }

    #[test]
    fn stable_reason_codes_cover_every_refusal_variant() {
        let refusals = [
            KmsLiveRefusal::SubcommandNotFirst,
            KmsLiveRefusal::UnknownArgument,
            KmsLiveRefusal::MissingDevice,
            KmsLiveRefusal::DuplicateDevice,
            KmsLiveRefusal::InvalidDevice,
            KmsLiveRefusal::MissingConnector,
            KmsLiveRefusal::DuplicateConnector,
            KmsLiveRefusal::InvalidConnector,
            KmsLiveRefusal::MissingScale,
            KmsLiveRefusal::DuplicateScale,
            KmsLiveRefusal::InvalidScale,
            KmsLiveRefusal::NonPositiveScale,
            KmsLiveRefusal::Non120thScale,
            KmsLiveRefusal::DuplicateFirstLight,
            KmsLiveRefusal::DuplicateKmsConfirm,
            KmsLiveRefusal::MissingPresentation,
            KmsLiveRefusal::DuplicatePresentation,
            KmsLiveRefusal::InvalidPresentation,
            KmsLiveRefusal::DirectDisplayRetired,
            KmsLiveRefusal::FeatureDisabled,
            KmsLiveRefusal::ReleaseBuildRequired,
            KmsLiveRefusal::TtyOpenFailed,
            KmsLiveRefusal::VtObservationUnavailable,
            KmsLiveRefusal::TtyNotCharacterDevice,
            KmsLiveRefusal::TtyNotKernelAlias,
            KmsLiveRefusal::TtyNotForegroundProcessGroup,
            KmsLiveRefusal::TtyNotVirtualTerminal,
            KmsLiveRefusal::TtyNotActive,
            KmsLiveRefusal::VtChangedSinceAuthorisation,
            KmsLiveRefusal::DeviceObservationUnavailable,
            KmsLiveRefusal::DeviceObservationTargetMismatch,
            KmsLiveRefusal::DeviceNotCharacterDevice,
            KmsLiveRefusal::DeviceNotPrimaryNode,
            KmsLiveRefusal::DeviceMissingUdevIdentity,
            KmsLiveRefusal::DeviceStableIdentityUnavailable,
            KmsLiveRefusal::DeviceStableIdentityChanged,
            KmsLiveRefusal::DeviceCanonicalIdentityChanged,
            KmsLiveRefusal::DeviceRdevMismatch,
            KmsLiveRefusal::SessionInactiveBeforeAuthorityOpen,
            KmsLiveRefusal::RevokedBeforeAuthorityOpen,
            KmsLiveRefusal::DrmNodeOpenFailed,
            KmsLiveRefusal::DrmNodeObservationUnavailable,
            KmsLiveRefusal::ConnectorBoundaryScanFailed,
            KmsLiveRefusal::ConnectorNotPresent,
            KmsLiveRefusal::TtyInputFlushFailed,
            KmsLiveRefusal::TtyLegacyInjectionStateUnavailable,
            KmsLiveRefusal::TtyLegacyInjectionEnabled,
            KmsLiveRefusal::ConfirmationNonceUnavailable,
            KmsLiveRefusal::ConfirmationReadFailed,
            KmsLiveRefusal::ConfirmationMismatch,
            KmsLiveRefusal::DeviceIncarnationOpenFailed,
            KmsLiveRefusal::DeviceIncarnationReadFailed,
            KmsLiveRefusal::DeviceIncarnationGone,
            KmsLiveRefusal::DeviceIncarnationChanged,
            KmsLiveRefusal::LiveBodyUnavailable,
        ];
        let codes = refusals
            .map(KmsLiveRefusal::reason_code)
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(codes.len(), refusals.len());
        assert!(codes.iter().all(|code| code.starts_with("kms-live-")));
    }

    #[test]
    fn primary_card_name_parser_excludes_render_and_connector_nodes() {
        assert!(is_primary_card_name(OsStr::new("card0")));
        assert!(!is_primary_card_name(OsStr::new("renderD128")));
        assert!(!is_primary_card_name(OsStr::new("card0-eDP-1")));
    }

    #[test]
    fn stable_identity_is_the_parent_device_derived_from_the_open_fd_sysfs_node() {
        let identity = open_drm_identity_from_sysfs(libc::makedev(226, 0), SYSFS_CARD.into())
            .expect("primary card sysfs identity");
        assert_eq!(identity.stable_device_path, Path::new(STABLE_DEVICE));
        assert_eq!(identity.sysfs_card_path, Path::new(SYSFS_CARD));
    }

    #[test]
    fn device_observation_maps_primary_udev_identity_and_card_connectors() {
        let request = parse_request(&argv()).expect("valid request");
        let rdev = libc::makedev(226, 0);
        let identity = device_identity_from_observation(
            &request,
            DeviceNodeObservation {
                canonical_path: DEVICE.into(),
                node_is_character_device: true,
                node_rdev: rdev,
                udev_sysname: Some("card0".into()),
                udev_rdev: Some(rdev),
                stable_device_path: Some(STABLE_DEVICE.into()),
            },
            [
                OsString::from("card0-eDP-1"),
                OsString::from("card0-HDMI-A-1"),
                OsString::from("card1-DP-1"),
            ],
        )
        .expect("valid observation");
        assert!(identity.node_is_primary_drm);
        assert_eq!(
            identity.stable_device_path.as_deref(),
            Some(Path::new(STABLE_DEVICE))
        );
        assert_eq!(
            identity.connectors,
            BTreeSet::from(["HDMI-A-1".into(), CONNECTOR.into()])
        );
    }

    #[test]
    fn build_profile_reflects_cargo_emitted_cfgs() {
        let current = BuildProfile::current();
        assert_eq!(current.kms_live_feature, cfg!(feature = "kms-live"));
        assert_eq!(
            current.release,
            cfg!(cosmix_kms_live_release) && env!("COSMIX_KMS_LIVE_CARGO_PROFILE") == "release"
        );
    }

    #[test]
    fn release_requires_both_cargo_markers() {
        assert!(!BuildProfile::from_build_markers(true, true, "not-release").release);
        assert!(!BuildProfile::from_build_markers(true, false, "release").release);
        assert!(BuildProfile::from_build_markers(true, true, "release").release);
    }

    #[test]
    fn controlling_tty_open_spec_is_exact() {
        assert_eq!(
            tty_open_spec(),
            TtyOpenSpec {
                read: true,
                write: true,
                custom_flags: libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_CLOEXEC,
            }
        );
    }

    #[test]
    fn production_confirmation_adapter_uses_input_flush_and_real_tty_io() {
        let kernel = Rc::new(RecordingTtyKernel::default());
        let tty_kernel: Rc<dyn TtyKernelCalls> = kernel.clone();
        let mut source = TtyConfirmationSource { tty_kernel };
        let (adapter_tty, mut peer) = UnixStream::pair().expect("tty test socket pair");
        let adapter_fd = adapter_tty.as_raw_fd();

        source
            .flush_input(adapter_tty.as_fd())
            .expect("recorded input flush succeeds");
        assert_eq!(
            kernel.calls.borrow().as_slice(),
            [RecordedTtyKernelCall::Flush {
                fd: adapter_fd,
                // `/usr/include/bits/termios.h:76` defines TCIFLUSH as 0.
                selector: 0,
            }]
        );

        source
            .display_prompt(adapter_tty.as_fd(), INTENT, CODE)
            .expect("real prompt writer succeeds");
        let expected_prompt = format!("{INTENT}\nType this code to continue: {CODE}\n");
        let mut prompt = vec![0; expected_prompt.len()];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        assert_eq!(prompt, expected_prompt.as_bytes());

        peer.write_all(format!("{CODE}\r\n").as_bytes())
            .expect("peer types confirmation");
        assert_eq!(
            source
                .read_line(adapter_tty.as_fd())
                .expect("real line reader succeeds"),
            CODE
        );
    }

    #[test]
    fn linux_vt_adapter_passes_the_guarded_fd_and_exact_ioctl_requests() {
        let kernel = RecordingTtyKernel::default();
        let tty = harmless_fd();
        let fd = tty.as_raw_fd();

        assert_eq!(
            observe_vt_after_fstat(
                &kernel,
                tty.as_fd(),
                true,
                libc::makedev(TTYAUX_MAJOR, TTY_ALIAS_MINOR),
            ),
            vt()
        );
        assert_eq!(
            kernel.calls.borrow().as_slice(),
            [
                RecordedTtyKernelCall::ForegroundProcessGroup { fd },
                RecordedTtyKernelCall::CallerProcessGroup,
                RecordedTtyKernelCall::TtyDevice {
                    fd,
                    request: libc::TIOCGDEV,
                },
                RecordedTtyKernelCall::VtState {
                    fd,
                    request: 0x5603,
                },
            ]
        );
    }

    #[test]
    fn a_non_foreground_caller_still_observes_the_vt_so_its_refusal_is_attributed() {
        let kernel = RecordingTtyKernel::default();
        kernel.caller_pgrp.set(31);
        let tty = harmless_fd();

        let observed = observe_vt_after_fstat(
            &kernel,
            tty.as_fd(),
            true,
            libc::makedev(TTYAUX_MAJOR, TTY_ALIAS_MINOR),
        );
        assert!(!observed.foreground_process_group);
        // The ioctls still ran: skipping them left `active_vt` unset, and
        // `validate_vt` checks that before the foreground flag — so the
        // dedicated foreground refusal was unreachable from production
        // observation and the failure was misreported as observation loss.
        assert!(observed.observation_available);
        assert!(observed.active_vt.is_some());
        assert_eq!(
            validate_vt(observed).expect_err("a non-foreground caller is refused"),
            KmsLiveRefusal::TtyNotForegroundProcessGroup,
            "the refusal names the foreground rule, not observation availability"
        );
    }

    #[test]
    fn controlling_tty_open_failure_has_a_sole_falsifier() {
        assert_eq!(
            require_open_tty::<OwnedFd>(None).expect_err("missing tty must refuse"),
            KmsLiveRefusal::TtyOpenFailed
        );
    }
}
