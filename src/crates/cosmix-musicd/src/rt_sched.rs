//! Realtime scheduling, memory locking, and the RT→async wake primitive.
//!
//! Three small pieces of Linux plumbing the mixer's audio path needs and had
//! been doing without:
//!
//! 1. [`promote_current_thread`] — `SCHED_FIFO` for the thread that actually
//!    runs the audio callback, so a busy desktop cannot preempt it into an
//!    xrun. Reference point: a shipping Rust DAW measured on this hardware runs
//!    24 threads at `SCHED_FIFO` priority 70.
//! 2. [`lock_process_memory`] — `mlockall(MCL_ONFAULT)`, so pages stay resident
//!    once they have been faulted in. `MCL_ONFAULT` deliberately does not
//!    pre-fault the whole address space: a first touch of a cold page in the
//!    callback can still fault from disk. That weaker guarantee is what makes
//!    locking affordable in a process that also does offline rendering.
//! 3. [`AudioWake`] — an `eventfd` the RT thread can signal without allocating,
//!    locking, or blocking, replacing an async-side poll
//!    (`_decisions/2026-07-20-no-poll-event-driven-amp-wake.md`).
//!
//! Everything here **fails soft**. An unprivileged container, a CI runner, or a
//! host without `RLIMIT_RTPRIO` must still start musicd — it just starts a run
//! that is not RT-scheduled, and says so loudly enough that a benchmark result
//! taken from it is not mistaken for a scheduled one.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

/// Default `SCHED_FIFO` priority for the audio callback thread. Matches the
/// priority a shipping DAW was measured using on this hardware, and sits well
/// under the `RLIMIT_RTPRIO` ceiling (99) so the promotion is grantable.
pub const DEFAULT_RT_PRIORITY: i32 = 70;

/// Env override for [`DEFAULT_RT_PRIORITY`]. `0` disables RT promotion entirely.
pub const ENV_RT_PRIORITY: &str = "MUSICD_RT_PRIORITY";

/// Env override for memory locking. `0` disables the `mlockall` attempt.
pub const ENV_MLOCK: &str = "MUSICD_MLOCK";

/// Property value used until an audio path has published its scheduling result.
pub const RT_PRIORITY_PENDING: i32 = -2;

/// The configured RT priority: [`DEFAULT_RT_PRIORITY`], or `MUSICD_RT_PRIORITY`
/// when it parses as a non-negative integer. `0` means "do not promote".
pub fn configured_rt_priority() -> i32 {
    match std::env::var(ENV_RT_PRIORITY) {
        Ok(s) => s
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|p| *p >= 0)
            .unwrap_or(DEFAULT_RT_PRIORITY),
        Err(_) => DEFAULT_RT_PRIORITY,
    }
}

/// Whether the `mlockall` attempt is enabled (default yes; `MUSICD_MLOCK=0` off).
pub fn mlock_enabled() -> bool {
    !matches!(std::env::var(ENV_MLOCK).as_deref(), Ok("0") | Ok("false"))
}

/// Default soft `RLIMIT_RTTIME`: 200 ms, matching what pipewire's `module-rt`
/// installs. At 256 frames/48 kHz a callback period is 5.3 ms, so this is ~38
/// consecutive missed periods — no legitimate audio path reaches it.
pub const DEFAULT_RT_TIME_US: u64 = 200_000;

/// Env override for [`DEFAULT_RT_TIME_US`]. `0` leaves `RLIMIT_RTTIME` alone.
pub const ENV_RT_TIME: &str = "MUSICD_RT_TIME_US";

/// The configured RT deadman-switch budget in microseconds; `0` disables it.
pub fn configured_rt_time_us() -> u64 {
    match std::env::var(ENV_RT_TIME) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(DEFAULT_RT_TIME_US),
        Err(_) => DEFAULT_RT_TIME_US,
    }
}

/// The soft `RLIMIT_RTTIME` to install, or `None` to leave the limit alone.
///
/// Pure, so the policy is testable without mutating the test process. Two rules:
/// a target of `0` means the watchdog is disabled, and an existing limit is
/// never *loosened*. The second matters because on the JACK path pipewire's
/// `module-rt` runs inside our own process and has already installed 200 ms —
/// raising that would weaken protection we did not install and, since it also
/// lowers the hard limit to match, would simply fail with EPERM anyway.
fn rt_time_soft_to_install(current_soft: libc::rlim_t, target_us: u64) -> Option<u64> {
    if target_us == 0 {
        return None;
    }
    if current_soft != libc::RLIM_INFINITY && current_soft <= target_us {
        return None;
    }
    Some(target_us)
}

/// Arm the RT deadman switch: a soft `RLIMIT_RTTIME` whose expiry raises
/// SIGXCPU on a thread that has burned that much CPU on an RT policy *without
/// making a blocking syscall*.
///
/// Only the soft limit is touched. pipewire sets soft and hard together; we do
/// not, because lowering a hard limit is irreversible for the process, and
/// leaving it alone means an operator can `prlimit` the soft limit back up to
/// inspect a thread that would otherwise be killed out from under them.
///
/// The cost is real and deliberate: SIGXCPU's default action terminates the
/// process. That is the point — the alternative is a wedged callback that holds
/// an RT priority forever, bounded only by the kernel's RT throttle, producing
/// no audio and no error. Under `Restart=on-failure` this trades a permanently
/// broken daemon for a five-second gap and a journal line naming the signal.
///
/// Returns the soft limit now in effect, or `None` when nothing was changed.
pub fn arm_rt_time_watchdog(target_us: u64) -> io::Result<Option<u64>> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid writable `rlimit` and RLIMIT_RTTIME is a valid
    // resource on Linux.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let Some(soft) = rt_time_soft_to_install(limit.rlim_cur, target_us) else {
        // Nothing to install, but something stricter may already be in force —
        // report what is actually protecting the thread, not what we did.
        return Ok(match limit.rlim_cur {
            libc::RLIM_INFINITY => None,
            current => Some(current),
        });
    };
    let next = libc::rlimit {
        rlim_cur: soft,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `next` is a valid initialised `rlimit`; soft <= hard holds because
    // `rt_time_soft_to_install` only ever lowers, and the hard limit is carried
    // through unchanged.
    if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &next) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(soft))
}

/// What the kernel actually did when the audio thread asked for RT.
///
/// Distinguishes "we promoted it" from "it arrived already promoted", because
/// the JACK path arrives already promoted and reporting that as our own grant
/// would misattribute the platform's work to us.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtPromotion {
    /// Promotion is disabled by configuration (`priority <= 0`).
    Disabled,
    /// The thread was already on an RT policy at or above the requested
    /// priority, so it was left alone.
    Inherited {
        /// The priority already in effect.
        priority: i32,
    },
    /// This call moved the thread onto `SCHED_FIFO`.
    Granted {
        /// The priority now in effect.
        priority: i32,
    },
}

impl RtPromotion {
    /// The priority now in effect; `0` when promotion is disabled.
    pub fn effective_priority(self) -> i32 {
        match self {
            RtPromotion::Disabled => 0,
            RtPromotion::Inherited { priority } | RtPromotion::Granted { priority } => priority,
        }
    }

    /// Whether the priority was already in effect rather than granted here.
    pub fn is_inherited(self) -> bool {
        matches!(self, RtPromotion::Inherited { .. })
    }
}

/// Promote the **calling** thread to `SCHED_FIFO` at `priority`.
///
/// Called from inside the audio callback on its first invocation, because the
/// cpal backend — not musicd — owns that thread; promoting the thread that
/// merely *builds* the stream would leave the real audio path untouched. One
/// syscall, once.
///
/// `priority <= 0` is a no-op returning [`RtPromotion::Disabled`].
///
/// It never *lowers* an existing RT priority. Under pipewire-jack the audio
/// callback runs on a thread libjack created and pipewire's `module-rt`
/// already promoted — measured `SCHED_FIFO` 83 on a host whose `@audio` group
/// carries an `rtprio` limit — and cpal's JACK backend spawns no thread of its
/// own. An unconditional `sched_setscheduler` there would not raise the audio
/// path to 70; it would drop it from 83, below every other JACK client on the
/// box. So: look before leaping.
pub fn promote_current_thread(priority: i32) -> io::Result<RtPromotion> {
    if priority <= 0 {
        return Ok(RtPromotion::Disabled);
    }
    if let Some(current) = current_rt_priority()
        && current >= priority
    {
        return Ok(RtPromotion::Inherited { priority: current });
    }
    // SAFETY: `param` is a valid initialised `sched_param`; pid 0 means "this
    // thread" for sched_setscheduler(2) on Linux.
    let rc = unsafe {
        let param = libc::sched_param {
            sched_priority: priority,
        };
        libc::sched_setscheduler(0, libc::SCHED_FIFO, &param)
    };
    if rc == 0 {
        Ok(RtPromotion::Granted { priority })
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The calling thread's RT priority, or `None` when it is not on an RT policy.
///
/// `SCHED_RR` counts as RT: pipewire's own data-loop runs round-robin, and a
/// higher-priority RR thread still outranks a `SCHED_FIFO` one, so treating RR
/// as "not RT" would reintroduce the demotion this guards against.
fn current_rt_priority() -> Option<i32> {
    // SAFETY: pid 0 means the calling thread; sched_getscheduler writes nothing.
    let policy = unsafe { libc::sched_getscheduler(0) };
    if policy < 0 {
        return None;
    }
    // The kernel or-s SCHED_RESET_ON_FORK into the value it returns; masking it
    // off is what makes the comparison below a policy test rather than a
    // coincidence.
    let policy = policy & !libc::SCHED_RESET_ON_FORK;
    if policy != libc::SCHED_FIFO && policy != libc::SCHED_RR {
        return None;
    }
    // SAFETY: `param` is a valid writable `sched_param` for the calling thread.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getparam(0, &mut param) } != 0 {
        return None;
    }
    Some(param.sched_priority)
}

/// The memory-locking mode selected from the process's soft `RLIMIT_MEMLOCK`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryLockMode {
    /// Lock current mappings on fault, but leave future mappings unlocked so a
    /// finite memlock limit cannot turn later allocation into a hard failure.
    CurrentOnFault,
    /// Lock current and future mappings on fault. Safe only with an unlimited
    /// soft memlock limit.
    CurrentAndFutureOnFault,
}

impl MemoryLockMode {
    fn flags(self) -> libc::c_int {
        match self {
            Self::CurrentOnFault => libc::MCL_CURRENT | libc::MCL_ONFAULT,
            Self::CurrentAndFutureOnFault => {
                libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT
            }
        }
    }
}

/// Select whether `MCL_FUTURE` is safe for a soft memlock limit.
fn memory_lock_mode_for_limit(soft_limit: libc::rlim_t) -> MemoryLockMode {
    if soft_limit == libc::RLIM_INFINITY {
        MemoryLockMode::CurrentAndFutureOnFault
    } else {
        MemoryLockMode::CurrentOnFault
    }
}

/// Lock mapped pages once they are faulted in.
///
/// `MCL_ONFAULT` does not populate cold pages, so their first touch can still
/// block; it guarantees only that a page stays resident after that touch.
/// `MCL_FUTURE` is added only when the soft `RLIMIT_MEMLOCK` is unlimited. With
/// a bounded limit, future mappings remain unlocked so a later allocation does
/// not fail with `ENOMEM` or turn stack growth into `SIGSEGV`.
///
/// Process-wide, so calling this more than once is harmless but pointless. The
/// returned mode lets the caller report exactly which guarantee was applied.
pub fn lock_process_memory() -> io::Result<MemoryLockMode> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid writable `rlimit`, and RLIMIT_MEMLOCK is a
    // valid resource selector on Linux.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mode = memory_lock_mode_for_limit(limit.rlim_cur);
    // Residual: this decision is valid for the limit observed immediately above.
    // A privileged external `prlimit` lowering RLIMIT_MEMLOCK after MCL_FUTURE is
    // enabled restores the later-allocation/stack-growth hazard. The process
    // cannot prevent an administrator from changing its resource limits.
    // SAFETY: mlockall only reads the validated flag word.
    let rc = unsafe { libc::mlockall(mode.flags()) };
    if rc == 0 {
        Ok(mode)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What the audio path actually got, as opposed to what it asked for.
///
/// Initialised and published by the first RT callback, with callback-size
/// variation updated by later callbacks. Readers take one coherent [`AudioRuntimeView`]
/// rather than loading independently changing fields.
#[derive(Debug, Default)]
pub struct AudioRuntime {
    /// Packed callback-size telemetry: maximum frames in the low 32 bits and the
    /// varied flag in bit 63. Private so callers cannot open-code the layout;
    /// [`AudioRuntime::view`] is the public read contract.
    block_frames_state: AtomicU64,
    /// Final `SCHED_FIFO` outcome once `primed` is true: `0` deliberately
    /// disabled/not attempted, `-1` attempted and refused, `>0` applied.
    rt_priority: AtomicI32,
    /// Raw `errno` from a refused promotion (`0` when there was none).
    rt_errno: AtomicI32,
    /// Whether `rt_priority` was already in effect when the audio path arrived
    /// (the JACK case) rather than granted by our own promotion. Written once,
    /// before the `primed` Release, so it falls under the same publication
    /// invariant as the fields above.
    rt_inherited: AtomicBool,
    /// Soft `RLIMIT_RTTIME` in force after arming, in microseconds; `0` when
    /// there is none. Same publication invariant as the fields above.
    rt_time_us: AtomicU64,
    /// Published last with Release once the first callback has written every
    /// runtime field. Readers use Acquire before trusting those fields.
    primed: AtomicBool,
    /// Priority to ask for on the first callback. Carried here rather than
    /// passed alongside so the audio path takes one parameter, not two that
    /// could ever be handed in mismatched.
    requested_priority: i32,
    /// Soft `RLIMIT_RTTIME` budget to arm on the first callback. Resolved from
    /// the environment up front for the same reason as `requested_priority`:
    /// the audio thread must not read the environment or allocate.
    requested_rt_time_us: u64,
}

/// One coherent observation of the audio path's runtime state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioRuntimeView {
    /// Whether an audio path has published its first complete result.
    pub observed: bool,
    /// Maximum callback frames observed; `0` when pending or on paced no-output.
    pub block_frames: u32,
    /// Whether a callback size has differed from the first observed size.
    pub block_frames_varied: bool,
    /// `-2` pending, `-1` refused, `0` disabled/not attempted, `>0` applied.
    pub rt_priority: i32,
    /// Raw errno for a refused promotion, otherwise zero.
    pub rt_errno: i32,
    /// Whether `rt_priority` was inherited from the platform (pipewire-jack
    /// promotes its own client thread) rather than granted by musicd.
    pub rt_inherited: bool,
    /// Soft `RLIMIT_RTTIME` protecting the audio thread, in microseconds; `0`
    /// when the RT deadman switch is not armed.
    pub rt_time_us: u64,
}

const BLOCK_FRAMES_VARIED_BIT: u64 = 1 << 63;
const BLOCK_FRAMES_MASK: u64 = u32::MAX as u64;

fn pack_block_frames(frames: u32, varied: bool) -> u64 {
    u64::from(frames) | if varied { BLOCK_FRAMES_VARIED_BIT } else { 0 }
}

fn unpack_block_frames(state: u64) -> (u32, bool) {
    (
        (state & BLOCK_FRAMES_MASK) as u32,
        state & BLOCK_FRAMES_VARIED_BIT != 0,
    )
}

impl AudioRuntimeView {
    const PENDING: Self = Self {
        observed: false,
        block_frames: 0,
        block_frames_varied: false,
        rt_priority: RT_PRIORITY_PENDING,
        rt_errno: 0,
        rt_inherited: false,
        rt_time_us: 0,
    };
}

impl AudioRuntime {
    /// A runtime that will ask for `priority` on its first callback. `0` or less
    /// means "do not promote" (see `promote_current_thread`).
    pub fn new(priority: i32) -> Self {
        Self {
            requested_priority: priority,
            requested_rt_time_us: configured_rt_time_us(),
            ..Self::default()
        }
    }

    /// One-time setup from inside the audio callback: promote this thread and
    /// record the first block size. Later calls track size variation and the
    /// maximum seen without allocating or making another syscall.
    ///
    /// Deliberately allocation-free and lock-free: it runs on the audio thread.
    #[inline]
    pub fn prime_from_callback(&self, frames: u32) {
        // Acquire is intentional even on the callback-side guard: once it sees
        // the first callback's Release, every published field is initialised
        // before this invocation updates the varying-size telemetry.
        if self.primed.load(Ordering::Acquire) {
            let previous_state = self.block_frames_state.load(Ordering::Relaxed);
            let (previous_frames, _) = unpack_block_frames(previous_state);
            if frames != previous_frames {
                let next_state = pack_block_frames(frames.max(previous_frames), true);
                if next_state != previous_state {
                    self.block_frames_state.store(next_state, Ordering::Relaxed);
                }
            }
            return;
        }
        self.publish_first_observation(frames);
    }

    /// Publish the scheduling outcome for the paced no-output thread. That
    /// thread is the audio path in fallback mode, but has no callback frames.
    pub fn prime_from_paced_path(&self) -> AudioRuntimeView {
        if !self.primed.load(Ordering::Acquire) {
            self.publish_first_observation(0);
        }
        self.view()
    }

    fn publish_first_observation(&self, frames: u32) {
        self.block_frames_state
            .store(pack_block_frames(frames, false), Ordering::Relaxed);
        // Arm the deadman switch BEFORE asking for RT, so this thread is never
        // on an RT policy without one. On the JACK path we are already RT by
        // the time we run — pipewire promoted this thread before handing it to
        // us — and arming is a no-op there because module-rt installed the same
        // 200 ms first; we still read back what is in force and report it.
        // Only when RT was actually asked for: RLIMIT_RTTIME bites threads on an
        // RT policy and nothing else, so arming it on a deliberately non-RT run
        // would report a protection that does not apply.
        if self.requested_priority > 0
            && let Ok(Some(us)) = arm_rt_time_watchdog(self.requested_rt_time_us)
        {
            self.rt_time_us.store(us, Ordering::Relaxed);
        }
        match promote_current_thread(self.requested_priority) {
            Ok(p) => {
                self.rt_priority
                    .store(p.effective_priority(), Ordering::Relaxed);
                self.rt_inherited.store(p.is_inherited(), Ordering::Relaxed);
            }
            Err(e) => {
                self.rt_priority.store(-1, Ordering::Relaxed);
                self.rt_errno
                    .store(e.raw_os_error().unwrap_or(0), Ordering::Relaxed);
            }
        }
        // Publication invariant: a reader that observes `primed` observes every
        // field the audio path wrote above.
        self.primed.store(true, Ordering::Release);
    }

    /// Whether the first callback has published a complete runtime record.
    pub fn is_primed(&self) -> bool {
        self.primed.load(Ordering::Acquire)
    }

    /// Take one coherent runtime observation.
    ///
    /// The Acquire load of `primed` is the single publication decision. Once it
    /// is true, `rt_priority` and `rt_errno` are final: the audio path wrote them
    /// once before the matching Release and never changes them. Callback-size
    /// maximum + variation are decoded from one packed Relaxed load, so a reader
    /// cannot combine a new maximum with an old varied flag.
    pub fn view(&self) -> AudioRuntimeView {
        if !self.primed.load(Ordering::Acquire) {
            return AudioRuntimeView::PENDING;
        }
        let (block_frames, block_frames_varied) =
            unpack_block_frames(self.block_frames_state.load(Ordering::Relaxed));
        AudioRuntimeView {
            observed: true,
            block_frames,
            block_frames_varied,
            rt_priority: self.rt_priority.load(Ordering::Relaxed),
            rt_errno: self.rt_errno.load(Ordering::Relaxed),
            rt_inherited: self.rt_inherited.load(Ordering::Relaxed),
            rt_time_us: self.rt_time_us.load(Ordering::Relaxed),
        }
    }

    /// Human-readable summary for the one-shot log line, or `None` if the audio
    /// callback has not run yet.
    pub fn describe(&self) -> Option<String> {
        let view = self.view();
        if !view.observed {
            return None;
        }
        let sched = match view.rt_priority {
            0 => "SCHED_OTHER (RT promotion disabled)".to_string(),
            -1 => {
                format!(
                    "SCHED_OTHER (SCHED_FIFO refused: {}) — this run is NOT RT-scheduled",
                    io::Error::from_raw_os_error(view.rt_errno)
                )
            }
            p if view.rt_inherited => format!(
                "RT prio {p}, inherited from the audio server (already above the \
                 requested {}) — not lowered",
                configured_rt_priority()
            ),
            p => format!("SCHED_FIFO prio {p}"),
        };
        let blocks = if view.block_frames_varied {
            format!(
                "block_frames_max={} (callback size varied)",
                view.block_frames
            )
        } else {
            format!("block_frames={} (stable so far)", view.block_frames)
        };
        let watchdog = match view.rt_time_us {
            0 => ", RT watchdog NOT armed (a wedged callback is bounded only by \
                  the kernel RT throttle)"
                .to_string(),
            us => format!(", RT watchdog {us} us (SIGXCPU)"),
        };
        Some(format!("{blocks}, {sched}{watchdog}"))
    }
}

/// An `eventfd` the RT thread signals and the async side awaits.
///
/// The RT half ([`AudioWake::signal`]) is one non-blocking 8-byte write: no
/// allocation, no userspace lock, and it cannot block even when nobody is
/// listening (the counter saturates and returns `EAGAIN`, which is exactly the
/// "already pending" case and is correctly ignored).
#[derive(Debug)]
pub struct AudioWake {
    fd: OwnedFd,
}

impl AudioWake {
    /// Create a non-blocking, close-on-exec eventfd.
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd(2) with a valid flag set; the returned fd is checked
        // before being adopted by `OwnedFd`.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh, valid, exclusively-owned descriptor.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Signal the waiter. **RT-safe**: one `write(2)`, never blocks, never
    /// allocates. A full counter (`EAGAIN`) means a wake is already pending, so
    /// the error is intentionally discarded.
    #[inline]
    pub fn signal(&self) {
        let v: u64 = 1;
        // SAFETY: writing exactly 8 bytes from a live `u64` to an eventfd, the
        // only write size eventfd accepts.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                std::ptr::from_ref(&v).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            );
        }
    }

    /// Consume all pending signals. Returns the coalesced count (`0` if none).
    /// Non-blocking; `EAGAIN` is reported as `0`.
    pub fn drain(&self) -> u64 {
        let mut v: u64 = 0;
        // SAFETY: reading exactly 8 bytes into a live `u64`, the only read size
        // eventfd accepts.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                std::ptr::from_mut(&mut v).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        if n == std::mem::size_of::<u64>() as isize {
            v
        } else {
            0
        }
    }
}

impl AsRawFd for AudioWake {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_signal_then_drain_coalesces() {
        let w = AudioWake::new().expect("eventfd");
        assert_eq!(w.drain(), 0, "a fresh eventfd has nothing pending");
        w.signal();
        w.signal();
        w.signal();
        assert_eq!(w.drain(), 3, "signals coalesce into one readable count");
        assert_eq!(w.drain(), 0, "drained eventfd is empty again");
    }

    /// The wrappers above are thin, so testing them alone would only prove they
    /// return what they were told. This drives the real syscalls on a spawned
    /// thread and reads the policy back from the kernel, which is what rules out
    /// the dangerous failure: a promotion reporting success while the thread
    /// stays on `SCHED_OTHER`. Both outcomes are legitimate — a host granting
    /// `RLIMIT_RTPRIO` promotes, a CI runner or unprivileged container is
    /// refused with `EPERM` — but "claimed success, policy unchanged" is not.
    #[test]
    fn promotion_either_takes_effect_or_is_refused() {
        let h = std::thread::spawn(|| {
            let before = unsafe { libc::sched_getscheduler(0) };
            assert_eq!(before, libc::SCHED_OTHER, "test thread started non-default");
            let result = promote_current_thread(DEFAULT_RT_PRIORITY);
            let after = unsafe { libc::sched_getscheduler(0) };
            (result, after)
        });
        let (result, after) = h.join().expect("promotion thread panicked");
        match result {
            Ok(p) => {
                // Which arm ran is the whole question on a new host, and a green
                // test alone cannot answer it.
                eprintln!("rt_sched: SCHED_FIFO promotion GRANTED at priority {p:?}");
                assert_eq!(
                    p,
                    RtPromotion::Granted {
                        priority: DEFAULT_RT_PRIORITY
                    },
                    "a thread that started on SCHED_OTHER cannot have inherited RT"
                );
                assert_eq!(
                    after,
                    libc::SCHED_FIFO,
                    "promote_current_thread reported success but the kernel still \
                     says the thread is on policy {after}"
                );
            }
            Err(e) => {
                eprintln!("rt_sched: SCHED_FIFO promotion REFUSED: {e}");
                assert_eq!(
                    e.raw_os_error(),
                    Some(libc::EPERM),
                    "the only expected refusal is EPERM (no RLIMIT_RTPRIO); got {e}"
                );
                assert_eq!(after, libc::SCHED_OTHER, "refused, yet the policy changed");
            }
        }
    }

    /// Same shape for `mlockall`: it either succeeds or is refused for a reason
    /// we can name. A silent success on a host that cannot lock memory would let
    /// a benchmark claim a guarantee it does not have.
    #[test]
    fn memory_locking_either_succeeds_or_names_its_refusal() {
        match lock_process_memory() {
            Ok(_mode) => {
                // Undo it: leaving the test harness with locked mappings would
                // distort every test that runs after this one.
                assert_eq!(unsafe { libc::munlockall() }, 0, "munlockall failed");
            }
            Err(e) => assert!(
                matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::ENOMEM)),
                "unexpected mlockall refusal: {e}"
            ),
        }
    }

    #[test]
    fn bounded_memlock_limit_excludes_mcl_future() {
        assert_eq!(
            memory_lock_mode_for_limit(0),
            MemoryLockMode::CurrentOnFault
        );
        assert_eq!(
            memory_lock_mode_for_limit(64 * 1024 * 1024),
            MemoryLockMode::CurrentOnFault
        );
        assert_eq!(
            memory_lock_mode_for_limit(libc::RLIM_INFINITY),
            MemoryLockMode::CurrentAndFutureOnFault
        );
    }

    #[test]
    fn rt_time_watchdog_installs_on_an_unlimited_process() {
        assert_eq!(
            rt_time_soft_to_install(libc::RLIM_INFINITY, DEFAULT_RT_TIME_US),
            Some(DEFAULT_RT_TIME_US)
        );
    }

    /// The JACK case: pipewire's module-rt already installed 200 ms in this very
    /// process. Installing our own target must not raise it.
    #[test]
    fn rt_time_watchdog_never_loosens_an_existing_limit() {
        assert_eq!(rt_time_soft_to_install(50_000, DEFAULT_RT_TIME_US), None);
        assert_eq!(
            rt_time_soft_to_install(DEFAULT_RT_TIME_US, DEFAULT_RT_TIME_US),
            None
        );
        // Stricter than what is in force is still allowed.
        assert_eq!(
            rt_time_soft_to_install(500_000, DEFAULT_RT_TIME_US),
            Some(DEFAULT_RT_TIME_US)
        );
    }

    #[test]
    fn rt_time_watchdog_is_disabled_by_a_zero_budget() {
        assert_eq!(rt_time_soft_to_install(libc::RLIM_INFINITY, 0), None);
        assert_eq!(rt_time_soft_to_install(50_000, 0), None);
    }

    /// Exercises the syscall path, not just the policy: after arming, the kernel
    /// must report a soft limit no looser than what was asked for, and the hard
    /// limit must be untouched so an operator can still raise the soft one back.
    #[test]
    fn arming_the_watchdog_is_visible_to_the_kernel() {
        let mut before = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut before) },
            0
        );
        let armed = arm_rt_time_watchdog(DEFAULT_RT_TIME_US).expect("arming failed");
        let mut after = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut after) },
            0
        );
        assert_ne!(
            after.rlim_cur,
            libc::RLIM_INFINITY,
            "a finite soft RLIMIT_RTTIME must be in force after arming"
        );
        assert!(
            after.rlim_cur <= DEFAULT_RT_TIME_US,
            "soft limit {} is looser than the {DEFAULT_RT_TIME_US} us asked for",
            after.rlim_cur
        );
        assert_eq!(
            after.rlim_max, before.rlim_max,
            "the hard limit must be carried through unchanged"
        );
        assert_eq!(armed, Some(after.rlim_cur));
    }

    #[test]
    fn zero_priority_is_a_no_op() {
        assert_eq!(
            promote_current_thread(0).expect("no-op"),
            RtPromotion::Disabled
        );
    }

    /// The JACK case, reproduced: pipewire-jack hands us a callback thread it
    /// has already promoted (measured `SCHED_FIFO` 83 against pipewire 1.6.8),
    /// so asking for our lower default must leave it alone. Without the guard
    /// in `promote_current_thread` this thread drops to `DEFAULT_RT_PRIORITY`
    /// and the assertions below fail.
    #[test]
    fn an_already_higher_rt_priority_is_never_lowered() {
        const ALREADY: i32 = DEFAULT_RT_PRIORITY + 10;
        let h = std::thread::spawn(|| {
            // Stand in for what pipewire's module-rt did to this thread.
            let setup = promote_current_thread(ALREADY);
            if let Err(e) = &setup {
                return Err(e.raw_os_error().unwrap_or(0));
            }
            let result = promote_current_thread(DEFAULT_RT_PRIORITY);
            let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::sched_getparam(0, &mut param) };
            Ok((result, rc, param.sched_priority))
        });
        match h.join().expect("demotion-guard thread panicked") {
            Ok((result, rc, kernel_priority)) => {
                assert_eq!(
                    result.expect("the second call must not fail"),
                    RtPromotion::Inherited { priority: ALREADY }
                );
                assert_eq!(rc, 0, "sched_getparam failed");
                assert_eq!(
                    kernel_priority, ALREADY,
                    "asking for {DEFAULT_RT_PRIORITY} lowered a thread already at {ALREADY}"
                );
            }
            Err(errno) => {
                // No RLIMIT_RTPRIO here, so the premise cannot be set up. Say so
                // rather than passing silently on an untested path.
                eprintln!(
                    "rt_sched: demotion guard NOT exercised — cannot reach RT priority \
                     {ALREADY} on this host ({})",
                    io::Error::from_raw_os_error(errno)
                );
                assert_eq!(errno, libc::EPERM, "the only expected refusal is EPERM");
            }
        }
    }

    #[test]
    fn configured_priority_defaults_when_unset() {
        // Only assert the default branch; the env var is process-global and
        // setting it here would race other tests in the same binary.
        if std::env::var(ENV_RT_PRIORITY).is_err() {
            assert_eq!(configured_rt_priority(), DEFAULT_RT_PRIORITY);
        }
    }

    #[test]
    fn audio_runtime_is_unknown_before_the_first_callback() {
        let r = AudioRuntime::default();
        assert_eq!(r.view(), AudioRuntimeView::PENDING);
        assert!(r.describe().is_none());
        r.prime_from_callback(512);
        assert_eq!(r.view().block_frames, 512);
        assert!(r.describe().expect("primed").contains("block_frames=512"));
    }

    #[test]
    fn primed_release_publishes_every_runtime_field() {
        // This can catch publishing `primed` before the other stores, but a
        // passing run on x86 does not by itself distinguish Release/Acquire from
        // Relaxed: TSO supplies stronger hardware ordering than Rust requires.
        let runtime = std::sync::Arc::new(AudioRuntime::new(i32::MAX));
        let reader = runtime.clone();
        let reader_thread = std::thread::spawn(move || {
            loop {
                let view = reader.view();
                if !view.observed {
                    std::hint::spin_loop();
                    continue;
                }
                assert_eq!(view.block_frames, 384);
                assert!(!view.block_frames_varied);
                assert_eq!(view.rt_priority, -1);
                assert_ne!(view.rt_errno, 0, "errno must differ from its default");
                // Armed before the promotion was attempted, so a refused
                // promotion still leaves the watchdog reported — this is the
                // ordering that guarantees no instant of RT without one.
                assert_ne!(
                    view.rt_time_us, 0,
                    "the RT watchdog must be armed before promotion is attempted"
                );
                break;
            }
        });
        let writer = runtime.clone();
        let writer_thread = std::thread::spawn(move || writer.prime_from_callback(384));
        writer_thread.join().expect("runtime writer");
        reader_thread.join().expect("runtime reader");
    }

    #[test]
    fn callback_size_variation_records_maximum_and_discloses_variation() {
        let r = std::sync::Arc::new(AudioRuntime::new(0));
        r.prime_from_callback(512);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let reader = r.clone();
        let reader_barrier = barrier.clone();
        let h = std::thread::spawn(move || {
            reader_barrier.wait();
            loop {
                let view = reader.view();
                match view.block_frames {
                    512 => assert!(!view.block_frames_varied),
                    1024 => {
                        assert!(
                            view.block_frames_varied,
                            "the packed load must not expose a new maximum as stable"
                        );
                        break;
                    }
                    other => panic!("impossible packed callback state: {other}"),
                }
                std::hint::spin_loop();
            }
        });
        barrier.wait();
        r.prime_from_callback(1024);
        h.join().expect("packed-state reader");
        r.prime_from_callback(256);
        let view = r.view();
        assert_eq!((view.block_frames, view.block_frames_varied), (1024, true));
        let desc = r.describe().expect("primed");
        assert!(desc.contains("block_frames_max=1024"));
        assert!(desc.contains("callback size varied"));
    }

    #[test]
    fn paced_path_publishes_a_non_default_scheduling_outcome() {
        let r = std::sync::Arc::new(AudioRuntime::new(i32::MAX));
        let writer = r.clone();
        let outcome = std::thread::spawn(move || writer.prime_from_paced_path())
            .join()
            .expect("paced-path writer");
        assert!(outcome.observed);
        assert_eq!(outcome.block_frames, 0);
        assert_eq!(outcome.rt_priority, -1);
        assert_ne!(outcome.rt_errno, 0);
        assert_eq!(r.view(), outcome);
    }
}
