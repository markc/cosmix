//! Device authorisation and the session-forwarding interface for libinput.
//!
//! # Why a forwarding interface exists at all
//!
//! Smithay ships [`LibinputSessionInterface`], which holds a [`Session`] and
//! forwards `open_restricted`/`close_restricted` straight to it
//! (`vendor/smithay/src/backend/libinput/mod.rs:697-719`). This compositor
//! cannot use it, and the reason is a hard type-level constraint rather than a
//! preference:
//!
//! - `LibSeatSession` holds a `std::rc::Weak<LibSeatSessionImpl>`
//!   (`vendor/smithay/src/backend/session/libseat.rs:45-49`), and the vendored
//!   tree carries no `unsafe impl Send` for it. It is therefore `!Send` and can
//!   **never** be moved or cloned onto another thread. `LibSeatSessionImpl`
//!   itself holds `RefCell<Seat>` and `RefCell<HashMap<RawFd, Device>>`
//!   (libseat.rs:31-35), which is the same statement made a second way.
//! - `LibinputInputBackend` is `!Send` too — it owns a raw `*mut libinput` and
//!   an `Rc<dyn LibinputInterface>` — so it must be *constructed on the thread
//!   that polls it*, which is the protocol thread.
//!
//! Those two facts cannot both be satisfied by one thread owning both objects,
//! because the session is already created on, and pinned to, the
//! `cosmix-kms-session` thread (`backend/kms_live.rs:1308`). So the split is
//! forced: **the session stays where it is, libinput is built on the protocol
//! thread, and the only things that cross between them are a device path going
//! one way and a file descriptor coming back.** Descriptors are `Send`; sessions
//! are not.
//!
//! Three comments elsewhere in this crate used to say E-5 would "move the
//! libseat session onto the protocol thread". That was never possible, and they
//! have been corrected rather than left as a plan nobody could execute.
//!
//! # What this costs, honestly
//!
//! Rung E's invariant is that the *event* path from device to seat has no
//! channel, queue or frame boundary in it (`protocol/input.rs:1-38`). This
//! design does not touch that path: events are read from the libinput fd on the
//! protocol thread and dispatched from the calloop callback exactly as before.
//!
//! But it would be wrong to file the *open* path under "setup only". Smithay's
//! `process_events` calls `self.context.dispatch()` before draining events
//! (`vendor/smithay/src/backend/libinput/mod.rs:735-741`), and libinput opens
//! devices from inside that dispatch whenever udev reports a new one or a
//! session resumes. So a device arriving at runtime makes the protocol thread
//! wait for a reply from the session thread, inside a calloop callback. Ordinary
//! input costs nothing; a hotplug or a resume costs one round trip. That is a
//! real risk this rung carries into E-5 rather than one it removes, and it is
//! named here so E-5 measures it instead of discovering it.
//!
//! # What is authorised, and why it is not the DRM gate
//!
//! Opens are not forwarded blindly. `open_authorised_session_device`
//! (`backend/kms_live.rs:472-513`) is the DRM gate, and it is *one-shot*: it
//! refuses when the authority is already `Open` and refuses when an original fd
//! is already retained. Routing input through it would not merely widen it —
//! after the DRM device is open, every input open would be refused outright, and
//! before it, the first input device would consume the slot the DRM node needs.
//! Input therefore gets its own repeatable predicate, [`authorise_input_open`],
//! which requires the DRM authority to already be `Open` rather than consuming
//! it.
//!
//! The predicate is deliberately narrow and fails closed. libinput asks for
//! whatever udev enumerated, so "it came from libinput" is not authorisation;
//! the compositor holds a libseat session that can open *any* device node it
//! names, and the whole point of a predicate is that a bug or a hostile udev
//! rule cannot turn "enumerate input devices" into "open `/dev/dri/card0`
//! again" or "open a tty".

use std::{
    os::fd::{BorrowedFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    path::{Component, Path},
    sync::mpsc,
};

use smithay::reexports::input::LibinputInterface;
use smithay::reexports::rustix::{
    self,
    fs::{FileType, OFlags, RawMode, major},
    io::FdFlags,
};

/// The Linux character-device major number for evdev nodes.
///
/// `/dev/input/event*` are all major 13. Checking it is what makes the name
/// check more than a spelling convention: a path is only a claim, while the
/// major of the node the path resolves to is what the kernel will actually hand
/// over.
const INPUT_DEVICE_MAJOR: u32 = 13;

/// Why an input open was refused.
///
/// Each variant is a distinct reason rather than one "denied", because these
/// become log lines on a machine with no display attached, and "the compositor
/// refused to open your keyboard" is not a diagnosis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputOpenRefusal {
    /// The session was revoked — a VT switch or a logind take-over.
    Revoked,
    /// libseat reports the session inactive.
    SessionInactive,
    /// The DRM authority is not open, so this is not a live session yet.
    DrmAuthorityNotOpen,
    /// The path is not exactly `/dev/input/eventN`.
    PathNotAnEventNode,
    /// The node exists but is not a character device.
    NodeNotACharacterDevice,
    /// The node is a character device of the wrong major.
    NodeNotInputMajor,
    /// The node could not be observed before opening.
    NodeNotObservable,
    /// The node opened is not the node that was inspected.
    NodeIdentityChangedAcrossOpen,
}

impl InputOpenRefusal {
    /// The errno libinput is told, since its interface speaks errno and nothing
    /// else (`input-0.9.1` `context.rs:25-35`).
    ///
    /// Two values, split by kind: a refusal about *authority* is `EACCES`,
    /// because retrying it without a session change is pointless; a refusal
    /// about the *node* is `ENODEV`, which is what libinput would have seen had
    /// the device genuinely not been there. Collapsing both into one value would
    /// make a policy refusal indistinguishable from an unplugged device in
    /// libinput's own retry behaviour.
    pub(crate) fn errno(self) -> i32 {
        match self {
            Self::Revoked | Self::SessionInactive | Self::DrmAuthorityNotOpen => {
                rustix_errno::ACCESS
            }
            Self::PathNotAnEventNode
            | Self::NodeNotACharacterDevice
            | Self::NodeNotInputMajor
            | Self::NodeNotObservable
            | Self::NodeIdentityChangedAcrossOpen => rustix_errno::NO_DEVICE,
        }
    }
}

/// The errno values this module returns, named rather than spelled inline.
///
/// Written as constants instead of `libc::EACCES` because this crate has no
/// `libc` dependency and adding one to name three integers would be the larger
/// change. The values are the Linux ABI's and cannot drift, and a test pins each
/// one against `rustix`'s own definition so a typo cannot pass unnoticed.
mod rustix_errno {
    pub(super) const ACCESS: i32 = 13;
    pub(super) const NO_DEVICE: i32 = 19;
    pub(super) const TIMED_OUT: i32 = 110;
}

/// What was observed about a device node, as facts rather than as a path.
///
/// Injected rather than read inside the predicate so the whole authorisation
/// decision is a pure function that runs under `cargo test` without a device.
/// The alternative — stat inside the predicate — would make every test of the
/// policy a test that needs `/dev/input` to exist and be shaped a particular
/// way, which is exactly the offline discipline this ladder is built on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InputNodeObservation {
    pub(crate) is_character_device: bool,
    /// The device number the node *names* — which device driver instance it
    /// refers to.
    pub(crate) rdev: u64,
    /// The filesystem the node itself lives on, and its identity within that
    /// filesystem. Carried alongside `rdev` because `rdev` alone does not
    /// identify a node: `/dev` is devtmpfs, evdev minors are **reused** when a
    /// device is unplugged and another appears, and a node that is removed and
    /// recreated at the same major:minor is a different filesystem object with
    /// the same `rdev`. Measured on this machine: two live evdev nodes share
    /// `st_dev` and differ by `st_ino`, and a removed-then-recreated node comes
    /// back with a fresh `st_ino`. Without these two, "the node I opened is the
    /// node I inspected" is a claim about a *number*, not about a node.
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl InputNodeObservation {
    fn major(self) -> u32 {
        major(self.rdev)
    }
}

/// Turn a raw `st_mode`/`st_rdev` pair into the facts the predicate wants.
///
/// Separated from the `stat` call itself so that everything the live path
/// decides is decided by a function this crate can run with no device present.
/// What is left in the live path is the `stat` call and nothing else.
pub(crate) fn observe_node(
    raw_mode: RawMode,
    rdev: u64,
    dev: u64,
    ino: u64,
) -> InputNodeObservation {
    InputNodeObservation {
        dev,
        ino,
        // `FileType`, not a mask comparison: `st_mode & S_IFCHR` is a classic
        // way to accidentally accept a socket or a block device, because
        // `S_IFCHR` is not a single bit and the file-type field is not a
        // bitfield.
        is_character_device: FileType::from_raw_mode(raw_mode) == FileType::CharacterDevice,
        rdev,
    }
}

/// The flags to hand `Session::open`, from the ones libinput asked for.
///
/// Forwarded verbatim, and it is worth knowing that today this is decoration:
/// `LibSeatSession::open` names the parameter `_flags` and never reads it
/// (`vendor/smithay/src/backend/session/libseat.rs:118`) — libseat's own
/// `open_device` decides. The request is carried anyway so that the intent is
/// recorded at the boundary rather than silently discarded at the caller, and
/// so a session implementation that *does* honour flags gets the right ones.
///
/// Because of that, close-on-exec cannot be *requested* here. It is asserted on
/// the descriptor afterwards instead — see [`ensure_close_on_exec`].
pub(crate) fn input_open_flags(requested: i32) -> OFlags {
    OFlags::from_bits_retain(requested as u32)
}

/// Make a descriptor close-on-exec, whatever it arrived as.
///
/// Not optional and not delegable. Without it a descriptor for every keyboard
/// and mouse on the machine is inherited by every process this compositor
/// spawns, and a compositor's whole job includes spawning clients. libseat very
/// likely sets it already; "very likely" is not a property, and the cost of
/// making it one is a single `fcntl`.
pub(crate) fn ensure_close_on_exec(fd: BorrowedFd<'_>) -> rustix::io::Result<()> {
    ensure_close_on_exec_with(
        fd,
        |fd| rustix::io::fcntl_getfd(fd),
        |fd, flags| rustix::io::fcntl_setfd(fd, flags),
    )
}

fn ensure_close_on_exec_with(
    fd: BorrowedFd<'_>,
    get_flags: impl FnOnce(BorrowedFd<'_>) -> rustix::io::Result<FdFlags>,
    set_flags: impl FnOnce(BorrowedFd<'_>, FdFlags) -> rustix::io::Result<()>,
) -> rustix::io::Result<()> {
    let existing = get_flags(fd)?;
    if existing.contains(FdFlags::CLOEXEC) {
        return Ok(());
    }
    set_flags(fd, existing | FdFlags::CLOEXEC)
}

/// Is this path exactly `/dev/input/eventN`?
///
/// Component-wise rather than by string prefix, and that is the whole point.
/// `starts_with("/dev/input")` accepts `/dev/input/../dri/card0`;
/// `path.ends_with("event0")` accepts `/tmp/evil/event0`; and matching on the
/// file name alone accepts `/dev/input/by-id/…-event-kbd` on some systems. This
/// requires the component sequence to be root, `dev`, `input`, `eventN` and
/// nothing else, so `..`, a relative path and an extra directory are rejected by
/// construction rather than by enumeration.
///
/// # What this check is not
///
/// It is **lexical**. It says nothing about symlinks in the *ancestor*
/// directories: if `/dev/input` were itself a symlink, the component sequence
/// would still read root, `dev`, `input`, `eventN`, and `lstat` would resolve
/// through it, because `lstat` declines to follow only the **final** component.
/// An earlier version of this comment claimed a symlinked directory was
/// "rejected by construction", which was simply false, and a cold review caught
/// it.
///
/// That is not a hole, because node identity is not established here. It is
/// established by [`verify_opened_input_node`], which compares the file type,
/// `rdev`, `dev` and `ino` of what libseat actually opened against what was
/// inspected — and that comparison holds however the ancestors resolved.
/// Canonicalising here would not help: libseat resolves the path it is given a
/// second time, so a canonical spelling proves nothing about what it opened.
/// What is *not* enforced, stated plainly, is any rule against ancestor
/// symlinks as such; that is not a requirement here.
///
/// It matches on components rather than on bytes, which means the spellings
/// `Path::components` folds together — a `.` segment, a doubled separator — are
/// accepted. That is the right set, not a concession: both name the same node
/// the plain spelling names. The one respelling that could name a *different*
/// node is `..`, and `Components` does **not** fold that away; it surfaces as
/// `Component::ParentDir` and fails the `Component::Normal` match below.
///
/// A **trailing separator** is the exception, and it is rejected explicitly
/// rather than accepted as an alias. `Components` folds it away, but the kernel
/// does not: a trailing slash asserts the target is a directory, and
/// `lstat("/dev/input/event0/")` fails with `ENOTDIR` — measured, not assumed.
/// Accepting it lexically would mean the predicate said yes to a spelling that
/// can never be opened, so the refusal would arrive later and under the wrong
/// name (`NodeNotObservable` rather than `PathNotAnEventNode`).
fn path_is_evdev_node(path: &Path) -> bool {
    // Checked on the bytes, because `Components` has already folded it away by
    // the time the loop below sees anything.
    if path.as_os_str().as_bytes().last() == Some(&b'/') {
        return false;
    }
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return false;
    }
    if components.next() != Some(Component::Normal("dev".as_ref())) {
        return false;
    }
    if components.next() != Some(Component::Normal("input".as_ref())) {
        return false;
    }
    let Some(Component::Normal(name)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(index) = name.strip_prefix("event") else {
        return false;
    };
    // Not `parse::<u32>()`: that accepts `+7`, and on some targets it would
    // accept a leading Unicode digit. The node names the kernel creates are
    // ASCII decimal, and an empty suffix — the bare name `event` — must be
    // rejected rather than treated as zero.
    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// Decide whether one input-device open may proceed.
///
/// Repeatable by design, unlike the DRM gate: libinput opens one node per device
/// and reopens them all after a resume, so a one-shot authority would authorise
/// the first keyboard and refuse the mouse.
///
/// `drm_authority_open` is required rather than consumed. It is the statement
/// "this process is the live session that already owns the display", and without
/// it an input open would be the compositor reaching for devices in a session it
/// has not established.
pub(crate) fn authorise_input_open(
    drm_authority_open: bool,
    revoked: bool,
    session_active: bool,
    path: &Path,
    observed: Option<InputNodeObservation>,
) -> Result<(), InputOpenRefusal> {
    // Revocation is checked before anything else and before liveness, so a
    // revoked session cannot be talked into an open by a node that happens to
    // look right.
    if revoked {
        return Err(InputOpenRefusal::Revoked);
    }
    if !session_active {
        return Err(InputOpenRefusal::SessionInactive);
    }
    if !drm_authority_open {
        return Err(InputOpenRefusal::DrmAuthorityNotOpen);
    }
    if !path_is_evdev_node(path) {
        return Err(InputOpenRefusal::PathNotAnEventNode);
    }
    let Some(observed) = observed else {
        return Err(InputOpenRefusal::NodeNotObservable);
    };
    if !observed.is_character_device {
        return Err(InputOpenRefusal::NodeNotACharacterDevice);
    }
    if observed.major() != INPUT_DEVICE_MAJOR {
        return Err(InputOpenRefusal::NodeNotInputMajor);
    }
    Ok(())
}

/// Confirm the descriptor libseat returned is the node that was authorised.
///
/// The predicate above inspects a path; libseat then opens that path. Between
/// the two, the name can be re-pointed at a different node — the ordinary
/// time-of-check/time-of-use gap. Comparing the identity seen through the *open
/// descriptor* against the one seen on the node closes it, because the
/// descriptor cannot be re-pointed after the fact. Identity here means the file
/// type, `rdev`, `dev` **and** `ino` together, for the reason given at the
/// comparison below: `rdev` alone names a device number, which is reused.
///
/// The caller must hand a rejected descriptor back through the session's
/// `close`, never merely drop it — see [`ForwardingLibinputInterface`].
pub(crate) fn verify_opened_input_node(
    authorised: InputNodeObservation,
    opened: Option<InputNodeObservation>,
) -> Result<(), InputOpenRefusal> {
    let Some(opened) = opened else {
        return Err(InputOpenRefusal::NodeNotObservable);
    };
    if !opened.is_character_device {
        return Err(InputOpenRefusal::NodeNotACharacterDevice);
    }
    // All three, not just `rdev`. `rdev` says which driver instance the node
    // refers to and is reused; `(dev, ino)` says which filesystem object was
    // actually opened. A node removed and recreated between the `lstat` and the
    // `fstat` — an unplug and replug that reuses the minor — matches on `rdev`
    // alone and is a different device.
    if opened.rdev != authorised.rdev
        || opened.dev != authorised.dev
        || opened.ino != authorised.ino
    {
        return Err(InputOpenRefusal::NodeIdentityChangedAcrossOpen);
    }
    Ok(())
}

/// The reply a device-opening thread sends back for one input open.
pub(crate) type InputOpenReply = mpsc::SyncSender<Result<OwnedFd, i32>>;

/// Create the reply channel for a single input open.
///
/// **Capacity zero, and the capacity is the point.** A buffered channel lets the
/// opening thread's `send` succeed into the buffer after the caller has stopped
/// waiting; the reply then sits in the buffer until the `Receiver` drops, and is
/// dropped with it. Dropping a descriptor that came from `LibSeatSession::open`
/// is precisely the leak [`LibinputDeviceTransport::close_input`] documents:
/// libseat files a `libseat::Device` in a map keyed by raw fd and only
/// `Session::close` removes it, so a drop closes the kernel fd and strands the
/// entry, after which the fd number can be recycled over it.
///
/// A zero-capacity channel is a rendezvous instead: a `send` succeeds only when
/// a receiver takes the value, so a `send` that *fails* has handed the
/// descriptor back, still owned, to a thread that can close it properly. That
/// makes a late reply safe by construction rather than by timing — see
/// [`deliver_open_reply`].
///
/// This is a function rather than an inline `sync_channel(0)` at the call site
/// so the capacity is stated once, next to the reason for it, and so a test can
/// hold the real channel and prove the hand-back actually happens.
pub(crate) fn input_open_reply_channel() -> (InputOpenReply, mpsc::Receiver<Result<OwnedFd, i32>>) {
    mpsc::sync_channel(0)
}

/// Send one open outcome to the waiting caller, reclaiming a descriptor nobody
/// took.
///
/// Takes the whole `Result` and the reclaim action together, because the thing
/// that must be true is a statement about *both* arms at once: a descriptor is
/// reclaimed exactly when it was opened and not delivered, and never otherwise.
/// Splitting them — sending here and reclaiming at the call site — is what
/// allowed the earlier `let _ = reply.send(Ok(fd));` to look reasonable.
///
/// `reclaim` is the session's own `close`, not test scaffolding: dropping a
/// descriptor that came from `LibSeatSession::open` closes the kernel fd while
/// stranding the `libseat::Device` in the session's map, after which the fd
/// number can be recycled over that live entry. An `Err` outcome carries no
/// descriptor, so there is nothing to reclaim whether it lands or not.
pub(crate) fn deliver_open_reply(
    reply: &InputOpenReply,
    outcome: Result<OwnedFd, i32>,
    reclaim: impl FnOnce(OwnedFd),
) {
    // Only the `Ok` arm can own a descriptor, so an undelivered errno matches
    // nothing here and is dropped, which is all it needs.
    if let Err(mpsc::SendError(Ok(fd))) = reply.send(outcome) {
        reclaim(fd);
    }
}

/// What to do when waiting for an open reply produced no reply.
///
/// The two ways a wait can fail look alike and must not be treated alike, which
/// is why this is a decision type rather than one errno.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InputOpenWaitFailure {
    /// Whether the transport must stop issuing opens from now on.
    pub(crate) poison: bool,
    /// The errno libinput is told.
    pub(crate) errno: i32,
}

/// Decide what a failed wait for an open reply means.
///
/// `Disconnected` says the opening thread is *gone*: the answer arrives
/// immediately, costs nothing, and is honest — there is no session, so there is
/// no device. It does not poison, because a torn-down session is not a wedged
/// one and nothing is gained by refusing to ask again.
///
/// `Timeout` says the opening thread is *alive but not progressing*, which is
/// the worse of the two. Every later open would wait the full deadline again and
/// stall the protocol thread for that long each time, so the transport stops
/// asking. `ETIMEDOUT` rather than `ENODEV` because the device may well be
/// there; what failed was this compositor, and the log line should not be able
/// to be mistaken for an unplugged keyboard.
pub(crate) fn classify_open_wait_failure(error: mpsc::RecvTimeoutError) -> InputOpenWaitFailure {
    match error {
        mpsc::RecvTimeoutError::Timeout => InputOpenWaitFailure {
            poison: true,
            errno: rustix_errno::TIMED_OUT,
        },
        mpsc::RecvTimeoutError::Disconnected => InputOpenWaitFailure {
            poison: false,
            errno: InputOpenRefusal::SessionInactive.errno(),
        },
    }
}

/// What a failed wait for an open reply means to the transport that waited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InputOpenWaitOutcome {
    /// The errno libinput is told.
    pub(crate) errno: i32,
    /// Whether this failure is the one that shut the gate.
    ///
    /// True at most once per gate. The transport raises its fatal signal on
    /// this and on nothing else, because later opens are refused before they are
    /// even sent and a signal keyed on "the gate is shut" would fire on every
    /// one of them. Decided here rather than by reading the gate either side of
    /// the record, so that the rule lives in a type every test can reach instead
    /// of in a transport no test compiles.
    pub(crate) newly_shut: bool,
}

/// Whether this transport may still ask the opening thread for devices.
///
/// This is the transport's actual state, not a description of it. It lives here
/// rather than as a bare `bool` field on the transport because the transport
/// itself is compiled only in a live build (`kms-live` and not `test`), so a
/// `bool` there would make the "one timeout stops all later asking" rule
/// unreachable by every test — the rule would be asserted about a classifier
/// while the field that enforces it went unchecked.
#[derive(Debug, Default)]
pub(crate) struct InputOpenGate {
    poisoned: bool,
}

impl InputOpenGate {
    /// The errno to return without asking at all, if this gate is shut.
    ///
    /// `None` means go ahead. Checked before the command is sent, because the
    /// point of poisoning is to avoid paying the deadline again, and a gate
    /// consulted after the wait would pay it every time.
    pub(crate) fn refusal_before_asking(&self) -> Option<i32> {
        self.poisoned.then_some(rustix_errno::TIMED_OUT)
    }

    /// Record a wait that produced no reply, and say what it means.
    ///
    /// Shuts the gate for good when the failure was a timeout. Never reopens it:
    /// a thread that stopped answering once has not been shown to have
    /// recovered, and re-asking costs the full deadline to find out.
    pub(crate) fn record_wait_failure(
        &mut self,
        error: mpsc::RecvTimeoutError,
    ) -> InputOpenWaitOutcome {
        let failure = classify_open_wait_failure(error);
        let newly_shut = failure.poison && !self.poisoned;
        if failure.poison {
            self.poisoned = true;
        }
        InputOpenWaitOutcome {
            errno: failure.errno,
            newly_shut,
        }
    }
}

/// Somewhere a device open or close can be performed on this compositor's
/// behalf.
///
/// Exactly two operations, because that is exactly what
/// [`LibinputInterface`] needs. Keeping the trait this small is what lets the
/// production implementation be a bare channel sender — no session handle, no
/// `Rc`, no raw pointer — and therefore `Send`, which is what allows it to be
/// built on the calling thread and moved into the factory closure that runs on
/// the protocol thread.
pub(crate) trait LibinputDeviceTransport {
    /// Open `path`, or return the errno libinput should see.
    fn open_input(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32>;

    /// Give a descriptor back to whoever opened it.
    ///
    /// Takes the `OwnedFd` **by value** and there is no return: this is a
    /// hand-back, not a request. Dropping the descriptor here instead of
    /// returning it is the leak this signature exists to prevent —
    /// `LibSeatSession::open` files the `libseat::Device` in a map keyed by raw
    /// fd (`libseat.rs:126-133`) and only `Session::close` removes it
    /// (`libseat.rs:141-161`). A dropped descriptor closes the kernel fd and
    /// leaves that entry behind, where a later open can be handed the same fd
    /// number and silently overwrite it.
    fn close_input(&mut self, fd: OwnedFd);
}

/// libinput's device interface, implemented by forwarding to a transport.
///
/// This type holds no libinput state and no session, which is what keeps it
/// `Send` when its transport is. Everything it does is delegate, and the
/// delegation is verbatim: the path and the flags libinput asked for are the
/// path and the flags the transport receives.
pub(crate) struct ForwardingLibinputInterface<T>(pub(crate) T);

impl<T: LibinputDeviceTransport> LibinputInterface for ForwardingLibinputInterface<T> {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        self.0.open_input(path, flags)
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        self.0.close_input(fd);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        os::fd::{AsFd, AsRawFd},
        os::unix::ffi::OsStrExt,
        path::PathBuf,
    };

    use smithay::reexports::rustix::{
        fs::{fstat, makedev},
        io::{Errno, dup, fcntl_getfd, fcntl_setfd},
        pipe::pipe,
    };

    use super::*;

    /// A descriptor to move around in tests, obtained without touching a device.
    ///
    /// A pipe rather than `/dev/null`: the point of these tests is that they run
    /// with no device node involved at all, and the strace gate that enforces it
    /// would rather see no `openat` of a device than one it has to allow-list.
    fn spare_fd() -> OwnedFd {
        let (read, _write) = pipe().expect("a pipe is available");
        read
    }

    /// A plausible live evdev node. The `dev`/`ino` values are arbitrary but
    /// non-zero and distinct, so a comparison that silently ignored either
    /// field would still have something to disagree about.
    fn evdev_observation() -> InputNodeObservation {
        InputNodeObservation {
            is_character_device: true,
            rdev: makedev(INPUT_DEVICE_MAJOR, 7),
            dev: 7,
            ino: 194,
        }
    }

    #[derive(Default)]
    struct FakeTransport {
        opened: Vec<(PathBuf, i32)>,
        closed: Vec<i32>,
        held: Vec<OwnedFd>,
        answer: Option<Result<OwnedFd, i32>>,
    }

    impl LibinputDeviceTransport for FakeTransport {
        fn open_input(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
            self.opened.push((path.to_path_buf(), flags));
            self.answer
                .take()
                .unwrap_or(Err(Errno::NODEV.raw_os_error()))
        }

        fn close_input(&mut self, fd: OwnedFd) {
            self.closed.push(fd.as_raw_fd());
            // Held rather than dropped, so a test can prove the descriptor
            // arrived open. A transport that closes on receipt would make
            // "still valid" unobservable and the assertion vacuous.
            self.held.push(fd);
        }
    }

    #[test]
    fn the_forwarder_passes_the_path_and_flags_through_unchanged() {
        let expected = spare_fd();
        let expected_raw = expected.as_raw_fd();
        let mut interface = ForwardingLibinputInterface(FakeTransport {
            answer: Some(Ok(expected)),
            ..FakeTransport::default()
        });

        let opened = interface
            .open_restricted(Path::new("/dev/input/event7"), 0o2)
            .expect("the fake transport answers this open");

        assert_eq!(
            interface.0.opened,
            vec![(PathBuf::from("/dev/input/event7"), 0o2)],
            "the transport sees the path and flags libinput asked for, unmodified"
        );
        assert_eq!(
            opened.as_raw_fd(),
            expected_raw,
            "libinput receives the transport's own descriptor rather than a copy of it"
        );
    }

    #[test]
    fn the_forwarder_reports_the_transports_errno_rather_than_a_substitute() {
        let mut interface = ForwardingLibinputInterface(FakeTransport {
            answer: Some(Err(InputOpenRefusal::Revoked.errno())),
            ..FakeTransport::default()
        });

        // Compared through `err()` because `OwnedFd` is not `PartialEq`, and
        // the success arm is not the subject here.
        assert_eq!(
            interface
                .open_restricted(Path::new("/dev/input/event0"), 0)
                .err(),
            Some(rustix_errno::ACCESS),
            "an authority refusal reaches libinput as EACCES, not as a generic failure"
        );
    }

    #[test]
    fn closing_hands_the_same_still_open_descriptor_back_to_the_transport() {
        let fd = spare_fd();
        let raw = fd.as_raw_fd();
        let mut interface = ForwardingLibinputInterface(FakeTransport::default());

        interface.close_restricted(fd);

        assert_eq!(
            interface.0.closed,
            vec![raw],
            "the descriptor libinput released is the one the transport is handed"
        );
        // The transport still holds it, so it must still be open. `fstat`
        // through the held descriptor is the check: if `close_restricted` had
        // dropped or duplicated-then-closed it, this is an `EBADF`. That is the
        // fact-5 leak in test form — a dropped descriptor closes the kernel fd
        // and strands the `libseat::Device` behind it.
        let held = interface.0.held.first().expect("the transport kept it");
        assert!(
            fstat(held).is_ok(),
            "the descriptor arrives at the transport still open, so libseat can close it properly"
        );
    }

    #[test]
    fn the_forwarding_interface_is_send_when_its_transport_is() {
        // The bound that matters: `InputSourceFactory` requires its closure to
        // be `Send` (`protocol/input.rs:339-343`), and the closure captures the
        // transport. `LibinputInputBackend` itself is `!Send` and deliberately
        // stays on the protocol thread — that half is enforced by the compiler
        // at the point of construction and is not asserted here, because a
        // negative trait bound is not expressible and a test that pretended to
        // check one would be checking nothing.
        fn require_send<T: Send>() {}
        require_send::<ForwardingLibinputInterface<FakeTransport>>();
    }

    #[test]
    fn only_an_exact_dev_input_event_node_is_accepted() {
        for accepted in [
            "/dev/input/event0",
            "/dev/input/event7",
            "/dev/input/event123",
            // Not a minor-range check: the kernel's numbering is its business,
            // and baking in today's limit would refuse a device on a machine
            // with more of them than this one has.
            "/dev/input/event4294967295",
            // Respellings `Path::components` folds together. Each names the
            // same node as `/dev/input/event0`, so refusing them would be a
            // false refusal that buys nothing — see the function's doc.
            "/dev/input/./event0",
            "/dev//input/event0",
        ] {
            assert!(
                path_is_evdev_node(Path::new(accepted)),
                "{accepted} is an evdev node"
            );
        }

        for rejected in [
            // Traversal, which a prefix test accepts and this must not. Unlike
            // `.`, a `..` is not folded away, so it survives to be rejected.
            "/dev/input/../dri/card0",
            "/dev/input/event0/../../dri/card0",
            "/dev/input/subdir/event0",
            // A trailing separator asserts "this is a directory", and the
            // kernel enforces that: `lstat("/dev/input/event0/")` returns
            // ENOTDIR, measured. `Components` folds the slash away, so this is
            // the one spelling the component walk alone would wrongly accept —
            // and accepting it would promise an open that can never happen.
            "/dev/input/event0/",
            // Other device classes reachable from the same session.
            "/dev/dri/card0",
            "/dev/tty0",
            "/dev/uinput",
            "/dev/hidraw0",
            // Legacy input aliases that are not evdev.
            "/dev/input/mice",
            "/dev/input/mouse0",
            "/dev/input/js0",
            // Stable-name symlink directories.
            "/dev/input/by-id/usb-kbd-event-kbd",
            "/dev/input/by-path/pci-0000:00:1d.0-event-mouse",
            // Name-shaped but not the node.
            "/dev/input/event",
            "/dev/input/event-1",
            "/dev/input/event+7",
            "/dev/input/event7a",
            "/dev/input/eventfoo",
            "/dev/input/Event7",
            // Right name, wrong place.
            "/tmp/input/event0",
            "dev/input/event0",
            "event0",
            "",
        ] {
            assert!(
                !path_is_evdev_node(Path::new(rejected)),
                "{rejected} is not an evdev node"
            );
        }
    }

    #[test]
    fn a_non_ascii_digit_suffix_is_not_a_node_index() {
        // `parse::<u32>()` would be the obvious implementation and would accept
        // this: Unicode decimal digits are not ASCII, and the kernel never names
        // a node with one.
        let arabic_indic_seven = OsStr::from_bytes("event\u{0667}".as_bytes());
        let path = Path::new("/dev/input").join(arabic_indic_seven);
        assert!(!path_is_evdev_node(&path));
    }

    #[test]
    fn an_open_is_refused_unless_the_session_is_live_and_already_owns_drm() {
        let path = Path::new("/dev/input/event0");
        let observed = Some(evdev_observation());

        assert_eq!(
            authorise_input_open(true, true, true, path, observed),
            Err(InputOpenRefusal::Revoked)
        );
        // The *order* of the checks, not just their presence. Every other case
        // here leaves exactly one thing wrong, so a predicate that tested them
        // in any order would satisfy them all. These leave several wrong at once
        // and pin which answer comes back: revocation is the most specific thing
        // that can be true of a session and must not be reported as mere
        // inactivity, which reads as "not yet" rather than "never again".
        assert_eq!(
            authorise_input_open(true, true, false, path, observed),
            Err(InputOpenRefusal::Revoked),
            "a revoked session is revoked, not merely inactive"
        );
        assert_eq!(
            authorise_input_open(false, true, false, path, observed),
            Err(InputOpenRefusal::Revoked),
            "revocation outranks every other refusal"
        );
        assert_eq!(
            authorise_input_open(false, false, false, path, observed),
            Err(InputOpenRefusal::SessionInactive),
            "liveness is decided before the DRM authority"
        );
        assert_eq!(
            authorise_input_open(false, false, true, Path::new("/dev/dri/card0"), observed),
            Err(InputOpenRefusal::DrmAuthorityNotOpen),
            "the authority is settled before the path is even looked at"
        );
        assert_eq!(
            authorise_input_open(true, false, false, path, observed),
            Err(InputOpenRefusal::SessionInactive)
        );
        assert_eq!(
            authorise_input_open(false, false, true, path, observed),
            Err(InputOpenRefusal::DrmAuthorityNotOpen),
            "input is opened by an established live session or not at all"
        );
        // The path is consulted even when the session is in perfect order and
        // the node inspects as a genuine evdev character device. This is the
        // case the guard exists for: `path_is_evdev_node` is tested directly
        // elsewhere, but nothing above pins that `authorise_input_open`
        // actually *calls* it, because every other authorised case here passes
        // a valid node path. Deleting the call would otherwise leave the whole
        // suite green while libinput could be talked into opening the card.
        assert_eq!(
            authorise_input_open(true, false, true, Path::new("/dev/dri/card0"), observed),
            Err(InputOpenRefusal::PathNotAnEventNode),
            "a live, authorised session still may not open something that is not an event node"
        );
        assert_eq!(
            authorise_input_open(true, false, true, path, observed),
            Ok(())
        );
    }

    #[test]
    fn an_open_is_refused_for_a_node_that_is_not_an_input_character_device() {
        let path = Path::new("/dev/input/event0");

        assert_eq!(
            authorise_input_open(true, false, true, path, None),
            Err(InputOpenRefusal::NodeNotObservable),
            "a node that could not be inspected is refused rather than assumed"
        );
        assert_eq!(
            authorise_input_open(
                true,
                false,
                true,
                path,
                Some(InputNodeObservation {
                    is_character_device: false,
                    ..evdev_observation()
                }),
            ),
            Err(InputOpenRefusal::NodeNotACharacterDevice),
            "a regular file or a symlink named like a node is not a node"
        );
        assert_eq!(
            authorise_input_open(
                true,
                false,
                true,
                path,
                Some(InputNodeObservation {
                    // 226 is the DRM major: the exact node this predicate exists
                    // to keep out, wearing an input node's name.
                    rdev: makedev(226, 0),
                    ..evdev_observation()
                }),
            ),
            Err(InputOpenRefusal::NodeNotInputMajor)
        );
    }

    #[test]
    fn the_opened_descriptor_must_be_the_node_that_was_authorised() {
        let authorised = evdev_observation();

        assert_eq!(
            verify_opened_input_node(authorised, Some(authorised)),
            Ok(())
        );
        assert_eq!(
            verify_opened_input_node(authorised, None),
            Err(InputOpenRefusal::NodeNotObservable)
        );
        assert_eq!(
            verify_opened_input_node(
                authorised,
                Some(InputNodeObservation {
                    // Same major, different device: the name was re-pointed at
                    // another input node between the check and the open.
                    rdev: makedev(INPUT_DEVICE_MAJOR, 8),
                    ..authorised
                }),
            ),
            Err(InputOpenRefusal::NodeIdentityChangedAcrossOpen),
            "the descriptor, not the path, decides which device was opened"
        );
        // The case `rdev` alone cannot see, and the reason this check carries
        // three fields instead of one. An evdev device is unplugged and another
        // appears; the kernel reuses the minor, so the replacement node has the
        // *same* `rdev`. It is nonetheless a different filesystem object, and
        // devtmpfs gives it a fresh inode — measured on this machine, where a
        // removed-and-recreated node came back with a new `st_ino`. Comparing
        // only `rdev` would call this the node that was inspected.
        assert_eq!(
            verify_opened_input_node(
                authorised,
                Some(InputNodeObservation {
                    ino: authorised.ino + 1,
                    ..authorised
                }),
            ),
            Err(InputOpenRefusal::NodeIdentityChangedAcrossOpen),
            "a node recreated at the same major:minor is not the node that was inspected"
        );
        // And the same statement for the filesystem the node lives on: a
        // different devtmpfs mount can hand out the same inode number.
        assert_eq!(
            verify_opened_input_node(
                authorised,
                Some(InputNodeObservation {
                    dev: authorised.dev + 1,
                    ..authorised
                }),
            ),
            Err(InputOpenRefusal::NodeIdentityChangedAcrossOpen),
            "the same inode number on another filesystem is another node"
        );
        assert_eq!(
            verify_opened_input_node(
                authorised,
                Some(InputNodeObservation {
                    is_character_device: false,
                    ..authorised
                }),
            ),
            Err(InputOpenRefusal::NodeNotACharacterDevice)
        );
    }

    #[test]
    fn a_refusal_tells_libinput_whether_retrying_could_ever_help() {
        for authority in [
            InputOpenRefusal::Revoked,
            InputOpenRefusal::SessionInactive,
            InputOpenRefusal::DrmAuthorityNotOpen,
        ] {
            assert_eq!(authority.errno(), rustix_errno::ACCESS, "{authority:?}");
        }
        for node in [
            InputOpenRefusal::PathNotAnEventNode,
            InputOpenRefusal::NodeNotACharacterDevice,
            InputOpenRefusal::NodeNotInputMajor,
            InputOpenRefusal::NodeNotObservable,
            InputOpenRefusal::NodeIdentityChangedAcrossOpen,
        ] {
            assert_eq!(node.errno(), rustix_errno::NO_DEVICE, "{node:?}");
        }
        // The two are distinct, which is the whole claim: collapsing them makes
        // a policy refusal indistinguishable from an absent device.
        assert_ne!(rustix_errno::ACCESS, rustix_errno::NO_DEVICE);
    }

    #[test]
    fn the_named_errnos_are_the_platform_errnos() {
        // The constants are written out because this crate has no `libc`. That
        // is only safe if they match, and this is what says so rather than a
        // comment claiming it.
        assert_eq!(rustix_errno::ACCESS, Errno::ACCESS.raw_os_error());
        assert_eq!(rustix_errno::NO_DEVICE, Errno::NODEV.raw_os_error());
    }

    #[test]
    fn an_observation_reads_the_major_the_kernel_encoded() {
        // `makedev`/`major` round-trip through the split encoding rather than
        // the low 8 bits, so a minor above 255 does not leak into the major.
        let observation = InputNodeObservation {
            rdev: makedev(INPUT_DEVICE_MAJOR, 300),
            ..evdev_observation()
        };
        assert_eq!(observation.major(), INPUT_DEVICE_MAJOR);
    }

    #[test]
    fn only_a_character_device_is_observed_as_one() {
        const PERMISSIONS: RawMode = 0o660;
        for (name, format) in [
            ("regular file", 0o100_000),
            ("directory", 0o040_000),
            ("block device", 0o060_000),
            ("fifo", 0o010_000),
            ("socket", 0o140_000),
            ("symlink", 0o120_000),
        ] {
            assert!(
                !observe_node(format | PERMISSIONS, 0, 0, 0).is_character_device,
                "a {name} is not a character device"
            );
        }
        assert!(observe_node(0o020_000 | PERMISSIONS, 0, 0, 0).is_character_device);
    }

    #[test]
    fn an_observation_keeps_the_device_number_it_was_given() {
        let rdev = makedev(INPUT_DEVICE_MAJOR, 64);
        assert_eq!(observe_node(0o020_660, rdev, 0, 0).rdev, rdev);
    }

    #[test]
    fn an_observation_keeps_the_node_identity_it_was_given() {
        // Distinct from each other and from `rdev`, so a constructor that
        // transposed two of the three would be visible here.
        let observed = observe_node(0o020_660, makedev(INPUT_DEVICE_MAJOR, 64), 7, 194);
        assert_eq!(observed.dev, 7, "the filesystem the node lives on");
        assert_eq!(observed.ino, 194, "the node's identity within it");
    }

    #[test]
    fn the_flags_libinput_asked_for_reach_the_session_unchanged() {
        for requested in [
            OFlags::RDONLY,
            OFlags::WRONLY,
            OFlags::RDWR,
            OFlags::RDWR | OFlags::NONBLOCK,
            OFlags::empty(),
        ] {
            assert_eq!(
                input_open_flags(requested.bits() as i32),
                requested,
                "the session is told what libinput asked for, neither more nor less"
            );
        }
    }

    #[test]
    fn a_descriptor_is_made_close_on_exec_whatever_it_arrived_as() {
        let fd = spare_fd();
        // Cleared first, because a pipe from `pipe()` is already CLOEXEC and a
        // test that starts in the desired state proves nothing about the code
        // meant to put it there.
        fcntl_setfd(&fd, FdFlags::empty()).expect("the flag can be cleared");
        assert!(
            !fcntl_getfd(&fd)
                .expect("readable")
                .contains(FdFlags::CLOEXEC)
        );

        ensure_close_on_exec(fd.as_fd()).expect("setting close-on-exec succeeds");
        assert!(
            fcntl_getfd(&fd)
                .expect("readable")
                .contains(FdFlags::CLOEXEC),
            "input descriptors must not be inherited by clients this compositor spawns"
        );

        // Idempotent: the already-set case must stay set rather than be toggled.
        ensure_close_on_exec(fd.as_fd()).expect("setting close-on-exec again succeeds");
        assert!(
            fcntl_getfd(&fd)
                .expect("readable")
                .contains(FdFlags::CLOEXEC)
        );
    }

    #[test]
    fn setting_close_on_exec_reports_a_bad_descriptor_rather_than_ignoring_it() {
        let fd = spare_fd();
        let raw = fd.as_raw_fd();
        let error = ensure_close_on_exec_with(
            fd.as_fd(),
            |observed| {
                assert_eq!(observed.as_raw_fd(), raw);
                Err(Errno::BADF)
            },
            |_, _| panic!("a failed flag read must not attempt a write"),
        );
        assert_eq!(error.err(), Some(Errno::BADF));
    }

    #[test]
    fn the_named_errno_constants_are_the_ones_rustix_defines() {
        // The module spells three errno values as integers rather than take a
        // `libc` dependency to name them. That is only safe if something checks
        // the integers, so this does — against `rustix`, which is already a
        // dependency and gets them from the ABI.
        assert_eq!(rustix_errno::ACCESS, Errno::ACCESS.raw_os_error());
        assert_eq!(rustix_errno::NO_DEVICE, Errno::NODEV.raw_os_error());
        assert_eq!(rustix_errno::TIMED_OUT, Errno::TIMEDOUT.raw_os_error());
    }

    #[test]
    fn an_unreceived_descriptor_is_reclaimed_exactly_once() {
        let (reply, result) = input_open_reply_channel();
        let fd = spare_fd();
        let raw = fd.as_raw_fd();

        // The caller gives up waiting, exactly as a timeout makes it do.
        drop(result);

        let mut reclaimed = Vec::new();
        deliver_open_reply(&reply, Ok(fd), |fd| {
            // Still open at the moment of reclamation: proving it arrived
            // *owned* rather than as a number that had already been closed.
            fcntl_getfd(&fd).expect("the reclaimed descriptor is still live");
            reclaimed.push(fd.as_raw_fd());
        });
        assert_eq!(
            reclaimed,
            [raw],
            "a descriptor nobody received must be reclaimed exactly once, so it goes back \
             through libseat instead of being dropped"
        );
    }

    #[test]
    fn a_received_descriptor_is_not_reclaimed() {
        let (reply, result) = input_open_reply_channel();
        let fd = spare_fd();
        let raw = fd.as_raw_fd();

        // A zero-capacity channel is a rendezvous, so the send blocks until a
        // receiver takes the value — the receive has to be on another thread.
        let receiver = std::thread::spawn(move || result.recv());
        let mut reclaimed = 0_u32;
        deliver_open_reply(&reply, Ok(fd), |_| reclaimed += 1);
        assert_eq!(
            reclaimed, 0,
            "a descriptor the caller received must not also be reclaimed — that would close \
             it out from under libinput"
        );
        let received = receiver
            .join()
            .expect("the receiving thread does not panic")
            .expect("the rendezvous delivered the descriptor")
            .expect("the reply carries the descriptor, not an errno");
        assert_eq!(
            received.as_raw_fd(),
            raw,
            "a descriptor that was received must reach the caller, not be reclaimed"
        );
    }

    #[test]
    fn an_errno_reply_never_reclaims_whether_or_not_it_arrives() {
        // The reclaim closure closes a device through libseat. Only the `Ok`
        // arm can own one, so an error reply must never reach it — running it
        // for an errno would be a close of something that was never opened.
        let (reply, result) = input_open_reply_channel();
        drop(result);
        let mut reclaimed = 0_u32;
        deliver_open_reply(
            &reply,
            Err(InputOpenRefusal::SessionInactive.errno()),
            |_| reclaimed += 1,
        );
        assert_eq!(reclaimed, 0, "an undelivered errno owns nothing to reclaim");

        let (reply, result) = input_open_reply_channel();
        let receiver = std::thread::spawn(move || result.recv());
        let mut reclaimed = 0_u32;
        deliver_open_reply(
            &reply,
            Err(InputOpenRefusal::NodeNotObservable.errno()),
            |_| reclaimed += 1,
        );
        assert_eq!(reclaimed, 0, "a delivered errno owns nothing to reclaim");
        let delivered = receiver
            .join()
            .expect("the receiving thread does not panic")
            .expect("the rendezvous delivered the reply");
        assert!(
            matches!(delivered, Err(errno) if errno == InputOpenRefusal::NodeNotObservable.errno()),
            "the errno reaches the caller unchanged"
        );
    }

    #[test]
    fn the_reply_channel_does_not_buffer_a_reply_nobody_takes() {
        // The property `an_unreceived_descriptor_comes_back_instead_of_being_dropped`
        // relies on is the channel's *capacity*, and that test would still pass
        // with a capacity-one channel as long as the receiver were dropped first
        // — the send would fail for want of a receiver either way. What a
        // buffered channel actually breaks is the case where the receiver is
        // still alive but no longer listening, so pin the capacity directly:
        // with a live receiver that never calls `recv`, a send must not succeed.
        let (reply, _result) = input_open_reply_channel();
        assert!(
            matches!(reply.try_send(Err(0)), Err(mpsc::TrySendError::Full(_))),
            "a buffered reply channel would swallow a descriptor the caller never takes"
        );
    }

    #[test]
    fn a_timed_out_wait_classifies_differently_from_a_disconnected_one() {
        let timed_out = classify_open_wait_failure(mpsc::RecvTimeoutError::Timeout);
        assert!(
            timed_out.poison,
            "an opening thread that is alive but not progressing would stall the protocol \
             thread for the full deadline on every later open"
        );
        assert_eq!(
            timed_out.errno,
            rustix_errno::TIMED_OUT,
            "a compositor that failed to answer must not be reported as a missing device"
        );

        let disconnected = classify_open_wait_failure(mpsc::RecvTimeoutError::Disconnected);
        assert!(
            !disconnected.poison,
            "a torn-down session answers instantly and costs nothing to ask again"
        );
        assert_eq!(
            disconnected.errno,
            InputOpenRefusal::SessionInactive.errno(),
            "no session means no device, and that is what libinput should be told"
        );

        assert_ne!(
            timed_out, disconnected,
            "the two failures must not collapse into one outcome"
        );
    }

    #[test]
    fn a_timed_out_wait_shuts_the_gate_for_good_and_a_disconnected_one_leaves_it_open() {
        let mut gate = InputOpenGate::default();
        assert_eq!(
            gate.refusal_before_asking(),
            None,
            "a fresh transport must ask the session thread rather than pre-refuse"
        );

        assert_eq!(
            gate.record_wait_failure(mpsc::RecvTimeoutError::Disconnected),
            InputOpenWaitOutcome {
                errno: InputOpenRefusal::SessionInactive.errno(),
                newly_shut: false,
            }
        );
        assert_eq!(
            gate.refusal_before_asking(),
            None,
            "a torn-down session answers instantly, so asking again costs nothing"
        );

        assert_eq!(
            gate.record_wait_failure(mpsc::RecvTimeoutError::Timeout),
            InputOpenWaitOutcome {
                errno: rustix_errno::TIMED_OUT,
                newly_shut: true,
            },
            "the first timeout is the transition, and the only one that may signal"
        );
        assert_eq!(
            gate.refusal_before_asking(),
            Some(rustix_errno::TIMED_OUT),
            "a thread that is alive but not progressing would stall the protocol thread for \
             the full deadline on every later open"
        );

        // Never cleared: a later disconnection must not reopen the gate, or the
        // stall returns the moment the wedged thread's channel is dropped.
        assert_eq!(
            gate.record_wait_failure(mpsc::RecvTimeoutError::Disconnected),
            InputOpenWaitOutcome {
                errno: InputOpenRefusal::SessionInactive.errno(),
                newly_shut: false,
            }
        );
        assert_eq!(
            gate.refusal_before_asking(),
            Some(rustix_errno::TIMED_OUT),
            "the gate is shut for good once an open has timed out"
        );
    }

    #[test]
    fn only_the_first_timeout_is_the_transition_that_may_signal() {
        // The transport's fatal signal is raised on `newly_shut` alone. A gate
        // that reported it on every timeout would signal the coordinator once
        // per stalled open, and one that never reported it would leave the
        // coordinator blocked on a thread that has stopped answering.
        let mut gate = InputOpenGate::default();
        assert!(
            gate.record_wait_failure(mpsc::RecvTimeoutError::Timeout)
                .newly_shut
        );
        for _ in 0..3 {
            assert!(
                !gate
                    .record_wait_failure(mpsc::RecvTimeoutError::Timeout)
                    .newly_shut,
                "a gate already shut is not shut again"
            );
        }
    }

    #[test]
    fn a_duplicated_descriptor_is_not_the_one_libinput_released() {
        // Guards the assertion in
        // `closing_hands_the_same_still_open_descriptor_back_to_the_transport`:
        // it compares raw fd numbers, which is only evidence if a duplicate has
        // a different number. If `dup` ever returned the same value that test
        // would pass against a `dup`-then-close implementation.
        let fd = spare_fd();
        let duplicate = dup(&fd).expect("dup succeeds on a live descriptor");
        assert_ne!(fd.as_raw_fd(), duplicate.as_raw_fd());
    }
}
