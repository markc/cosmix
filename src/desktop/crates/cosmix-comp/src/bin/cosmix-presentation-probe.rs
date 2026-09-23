//! Minimal SHM client for the public `wp_presentation` contract.
//!
//! Maps one xdg toplevel and, for each of `--frames` frame callbacks,
//! commits `--burst` buffers back to back. Every commit of a single-commit
//! `--callbacks-only` measures just the compositor frame-callback cadence,
//! the one measurement that also works against a compositor without
//! wp_presentation (a host, for the nested pacing work).
//!
//! frame asks for presentation feedback; in burst mode the first and the
//! last commit of each burst ask, so superseded commits are exercised too.
//! Prints one `COSMIX_PRESENTATION_PROBE` summary line and exits non-zero
//! when an assertion fails.
//!
//! By default the nested contract is asserted (flags, seq and refresh all
//! zero). `--expect-kms` asserts the KMS one instead (see `KmsExpectation`),
//! against `--expect-kms-refresh-ns N` or else the current mode of the
//! output the frames were presented on; `--expect-output NAME` also requires
//! that output to be the one named.

use smithay::reexports::wayland_protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback,
};
use smithay::reexports::wayland_protocols::xdg::shell::client::{
    xdg_surface, xdg_toplevel, xdg_wm_base,
};
use std::{
    env,
    ffi::CString,
    fs::File,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd},
        unix::fs::FileExt,
    },
    process::ExitCode,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Presented {
        time: Duration,
        refresh_ns: u32,
        seq: u64,
        flags: u32,
        /// The bound output (index into `Probe::outputs`) a `sync_output`
        /// named before `presented`.
        synced: Option<usize>,
    },
    Discarded,
}

#[derive(Default)]
struct FeedbackSlot {
    synced: Option<usize>,
    outcome: Option<Outcome>,
}

#[derive(Default)]
struct Probe {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    presentation: Option<wp_presentation::WpPresentation>,
    outputs: Vec<wl_output::WlOutput>,
    /// Per bound output: `wl_output.name` and the current mode's refresh.
    output_info: Vec<OutputInfo>,
    clock_id: Option<u32>,
    configured: Option<u32>,
    frame_done: bool,
    /// Indexed by commit number; only commits that asked have a slot used.
    feedback: Vec<FeedbackSlot>,
    closed: bool,
    failure: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    name: Option<String>,
    refresh_mhz: Option<u32>,
}

fn monotonic_now() -> Duration {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is valid writable storage for one timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) };
    Duration::new(timestamp.tv_sec as u64, timestamp.tv_nsec as u32)
}

fn dispatch_until(
    queue: &mut EventQueue<Probe>,
    probe: &mut Probe,
    deadline: Instant,
    complete: impl Fn(&Probe) -> bool,
    phase: &str,
) -> Result<(), String> {
    while !complete(probe) {
        queue
            .dispatch_pending(probe)
            .map_err(|error| format!("{phase} dispatch failed: {error}"))?;
        if complete(probe) || probe.closed || probe.failure.is_some() {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        queue
            .flush()
            .map_err(|error| format!("failed to flush {phase} request: {error}"))?;
        let Some(read_guard) = queue.prepare_read() else {
            continue;
        };
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd: read_guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: descriptor is valid writable storage for one pollfd and the
        // read guard keeps the Wayland connection fd alive during poll.
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == 0 {
            drop(read_guard);
            continue;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            drop(read_guard);
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("polling Wayland fd during {phase} failed: {error}"));
        }
        if descriptor.revents & libc::POLLIN != 0 {
            read_guard.read().map_err(|error| {
                format!("reading Wayland events during {phase} failed: {error}")
            })?;
        } else {
            drop(read_guard);
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err(format!("Wayland compositor disconnected during {phase}"));
            }
        }
    }
    if let Some(failure) = &probe.failure {
        return Err(format!("{failure} (during {phase})"));
    }
    if probe.closed {
        return Err(format!("toplevel closed by the compositor during {phase}"));
    }
    complete(probe)
        .then_some(())
        .ok_or_else(|| format!("{phase} did not complete in time"))
}

struct Options {
    frames: usize,
    burst: usize,
    width: u32,
    height: u32,
    timeout: Duration,
    /// Keep the window mapped this long after the result line, so a gate
    /// can read the compositor's stats for it (G1+).
    hold: Duration,
    /// Measure only the compositor's frame-callback cadence, without
    /// wp_presentation: the one measurement that works against a
    /// compositor that does not advertise it.
    callbacks_only: bool,
    /// Assert the KMS contract instead of the nested one.
    expect_kms: bool,
    /// The KMS mode period; default: the presented output's current mode.
    kms_refresh_ns: Option<u32>,
    /// The `wl_output.name` every presented commit must be synced to.
    expect_output: Option<String>,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        frames: 300,
        burst: 1,
        width: 256,
        height: 256,
        timeout: Duration::from_secs(60),
        hold: Duration::ZERO,
        callbacks_only: false,
        expect_kms: false,
        kms_refresh_ns: None,
        expect_output: None,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--frames" => {
                options.frames = value()?
                    .parse()
                    .map_err(|error| format!("--frames: {error}"))?;
            }
            "--burst" => {
                options.burst = value()?
                    .parse()
                    .map_err(|error| format!("--burst: {error}"))?;
            }
            "--size" => {
                let size = value()?;
                let (width, height) = size
                    .split_once('x')
                    .ok_or_else(|| "--size expects WxH".to_string())?;
                options.width = width.parse().map_err(|error| format!("--size: {error}"))?;
                options.height = height.parse().map_err(|error| format!("--size: {error}"))?;
            }
            "--timeout-s" => {
                options.timeout = Duration::from_secs(
                    value()?
                        .parse()
                        .map_err(|error| format!("--timeout-s: {error}"))?,
                );
            }
            "--callbacks-only" => options.callbacks_only = true,
            "--hold-s" => {
                options.hold = Duration::from_secs(
                    value()?
                        .parse()
                        .map_err(|error| format!("--hold-s: {error}"))?,
                );
            }
            "--expect-kms" => options.expect_kms = true,
            "--expect-kms-refresh-ns" => {
                options.expect_kms = true;
                options.kms_refresh_ns = Some(
                    value()?
                        .parse()
                        .map_err(|error| format!("--expect-kms-refresh-ns: {error}"))?,
                );
            }
            "--expect-output" => options.expect_output = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.frames == 0 || options.burst == 0 || options.width == 0 || options.height == 0 {
        return Err("frames, burst and size must be non-zero".into());
    }
    Ok(options)
}

/// Which commits ask for feedback: all of them without bursts; otherwise
/// the first (superseded) and last (shown) commit of each burst.
fn asks_feedback(commit: usize, burst: usize) -> bool {
    burst == 1 || commit.is_multiple_of(burst) || commit % burst == burst - 1
}

fn is_last_of_burst(commit: usize, burst: usize) -> bool {
    commit % burst == burst - 1
}

/// One presented commit: `(tv, refresh ns, seq, flags)`.
type PresentedCommit = (Duration, u32, u64, u32);

/// The KMS contract a steady client (one commit per frame callback) sees:
/// every presented commit names its own page flip, with the flags a
/// completed flip on a MONOTONIC kernel clock proves, the mode's refresh,
/// about one refresh apart, and seq steps that match the tv gaps.
#[derive(Debug, PartialEq, Eq)]
struct KmsExpectation {
    refresh_ns: Option<u32>,
    seq_increasing: bool,
    flags_kms: bool,
    refresh_ok: bool,
    interval_ok: bool,
    seq_tv_mismatch: usize,
}

impl KmsExpectation {
    /// VSYNC | HW_CLOCK | HW_COMPLETION.
    const FLAGS: u32 = VSYNC | HW_CLOCK | HW_COMPLETION;

    fn check(presented: &[PresentedCommit], refresh_ns: Option<u32>, interval_p50_us: u64) -> Self {
        let refresh_ns = refresh_ns.filter(|refresh| *refresh > 0);
        let seq_increasing = presented.windows(2).all(|pair| pair[1].2 > pair[0].2);
        let flags_kms = presented.iter().all(|(.., flags)| *flags == Self::FLAGS);
        let (refresh_ok, interval_ok, seq_tv_mismatch) = match refresh_ns {
            None => (false, false, 0),
            Some(refresh) => {
                let tolerance = (refresh / 1000).max(1);
                let refresh_ok = presented
                    .iter()
                    .all(|(_, reported, ..)| reported.abs_diff(refresh) <= tolerance);
                let refresh_us = u64::from(refresh) / 1000;
                let interval_ok = interval_p50_us.abs_diff(refresh_us) <= refresh_us / 10;
                // A gap of k refresh periods is a sequence step of k.
                let mismatch = presented
                    .windows(2)
                    .filter(|pair| {
                        let periods = (pair[1].0.saturating_sub(pair[0].0).as_nanos() as f64
                            / f64::from(refresh))
                        .round() as u64;
                        pair[1].2.saturating_sub(pair[0].2) != periods
                    })
                    .count();
                (refresh_ok, interval_ok, mismatch)
            }
        };
        Self {
            refresh_ns,
            seq_increasing,
            flags_kms,
            refresh_ok,
            interval_ok,
            seq_tv_mismatch,
        }
    }

    fn pass(&self) -> bool {
        self.refresh_ns.is_some()
            && self.seq_increasing
            && self.flags_kms
            && self.refresh_ok
            && self.interval_ok
            && self.seq_tv_mismatch == 0
    }

    fn summary(&self) -> String {
        format!(
            "refresh_ns={} seq_increasing={} flags_kms={} refresh_ok={} interval_ok={} \
             seq_tv_mismatch={}",
            self.refresh_ns
                .map_or_else(|| "unknown".to_string(), |refresh| refresh.to_string()),
            self.seq_increasing,
            self.flags_kms,
            self.refresh_ok,
            self.interval_ok,
            self.seq_tv_mismatch,
        )
    }
}

/// wp_presentation `kind` bits.
const VSYNC: u32 = 0x1;
const HW_CLOCK: u32 = 0x2;
const HW_COMPLETION: u32 = 0x4;

/// Every presented timestamp must lie inside the probe's own
/// `CLOCK_MONOTONIC` window `[started, finished]`, where `finished` is read
/// after the LAST feedback event resolved. A `HW_CLOCK` timestamp with a
/// refresh is allowed to LEAD `finished` by less than HALF a refresh period:
/// a DRM flip event is delivered from the vblank interrupt carrying the
/// vblank-edge time computed from the scanout position, which sits a few
/// scanlines ahead of the interrupt (measured 42–65 µs on i915 at 60 Hz,
/// 2026-09-18) and can never approach a full period. Half a period is the
/// widest bound that still rejects a compositor stamping the NEXT vblank for
/// a completed flip: that stamp leads by one period minus the same few
/// scanlines, and no other check sees a uniform one-period shift
/// (`increasing`, the seq/tv steps and the refresh all survive it, and a
/// future stamp cannot trip `tv_before_commit`). Samples without `HW_CLOCK`
/// or without a refresh get no allowance: they are stamped before the event
/// is sent and cannot lead. Returns the verdict and the largest lead over
/// EVERY sample (no short-circuit), which the report prints as
/// `window_lead_us`; a stamp before `started` fails and counts no lead.
fn window_check(
    presented: &[PresentedCommit],
    started: Duration,
    finished: Duration,
) -> (bool, u64) {
    presented.iter().fold(
        (true, 0_u64),
        |(ok, max_lead), (time, refresh, _, flags)| {
            if *time < started {
                return (false, max_lead);
            }
            let lead = time.saturating_sub(finished).as_nanos() as u64;
            let allowed = if *flags & HW_CLOCK != 0 {
                u64::from(*refresh) / 2
            } else {
                0
            };
            (ok && lead < allowed.max(1), max_lead.max(lead))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: u32 = 16_666_667;

    #[test]
    fn hardware_timestamps_may_lead_the_window_by_under_half_a_refresh() {
        let started = Duration::from_secs(100);
        let finished = started + Duration::from_millis(50);
        let flip = |lead_ns: u64, flags: u32| {
            vec![(finished + Duration::from_nanos(lead_ns), R, 7_u64, flags)]
        };
        let half = u64::from(R) / 2;
        let (ok, lead) = window_check(&flip(65_000, KmsExpectation::FLAGS), started, finished);
        assert!(ok, "65 µs lead under HW_CLOCK passes");
        assert_eq!(lead, 65_000);
        let (ok, _) = window_check(&flip(half - 1, KmsExpectation::FLAGS), started, finished);
        assert!(ok, "just under half a refresh is still a vblank edge");
        let (ok, lead) = window_check(&flip(half, KmsExpectation::FLAGS), started, finished);
        assert!(!ok, "half a refresh is the bound (exclusive)");
        assert_eq!(lead, half);
        // A compositor stamping the NEXT vblank for a completed flip leads by
        // one period minus the few scanlines the edge sits ahead of the IRQ.
        let next_vblank = u64::from(R) - 65_000;
        let (ok, _) = window_check(&flip(next_vblank, KmsExpectation::FLAGS), started, finished);
        assert!(!ok, "a next-vblank stamp must not pass as a vblank edge");
    }

    #[test]
    fn the_reported_lead_is_the_maximum_over_every_sample() {
        let started = Duration::from_secs(100);
        let finished = started + Duration::from_millis(50);
        let at = |lead_ns: u64| {
            (
                finished + Duration::from_nanos(lead_ns),
                R,
                7_u64,
                KmsExpectation::FLAGS,
            )
        };
        // Fails at the second sample; the largest lead is the third.
        let train = vec![at(10_000), at(u64::from(R)), at(20_000_000)];
        let (ok, lead) = window_check(&train, started, finished);
        assert!(!ok);
        assert_eq!(lead, 20_000_000, "no short-circuit at the first failure");
        let passing = vec![at(0), at(40_000), at(65_000), at(12_000)];
        assert_eq!(window_check(&passing, started, finished), (true, 65_000));
    }

    #[test]
    fn software_timestamps_get_no_lead_allowance() {
        let started = Duration::from_secs(100);
        let finished = started + Duration::from_millis(50);
        let one_us_late = vec![(finished + Duration::from_micros(1), R, 7_u64, 0x1)];
        assert!(
            !window_check(&one_us_late, started, finished).0,
            "no HW_CLOCK: 1 µs lead fails"
        );
        let no_refresh = vec![(
            finished + Duration::from_micros(1),
            0,
            7_u64,
            KmsExpectation::FLAGS,
        )];
        assert!(
            !window_check(&no_refresh, started, finished).0,
            "HW_CLOCK without a refresh: no allowance"
        );
        let inside = vec![(finished, R, 7_u64, 0x1), (started, R, 8_u64, 0x1)];
        assert!(
            window_check(&inside, started, finished).0,
            "the closed window itself passes"
        );
    }

    #[test]
    fn timestamps_before_the_window_fail_regardless_of_flags() {
        let started = Duration::from_secs(100);
        let finished = started + Duration::from_millis(50);
        let early = vec![(
            started - Duration::from_nanos(1),
            R,
            7_u64,
            KmsExpectation::FLAGS,
        )];
        let (ok, lead) = window_check(&early, started, finished);
        assert!(!ok);
        assert_eq!(lead, 0, "an early stamp is not a lead");
    }

    /// Flips `periods[i]` refresh periods after the previous one, with the
    /// sequence advancing by the same amount.
    fn train(periods: &[u64]) -> Vec<PresentedCommit> {
        let mut tv = Duration::from_secs(100);
        let mut seq = 1_000;
        let mut commits = vec![(tv, R, seq, KmsExpectation::FLAGS)];
        for step in periods {
            tv += Duration::from_nanos(u64::from(R) * step);
            seq += step;
            commits.push((tv, R, seq, KmsExpectation::FLAGS));
        }
        commits
    }

    #[test]
    fn a_steady_flip_train_passes() {
        let check = KmsExpectation::check(&train(&[1, 1, 2, 1]), Some(R), 16_666);
        assert!(check.pass(), "{check:?}");
    }

    #[test]
    fn each_kms_claim_is_checked() {
        let steady = train(&[1, 1, 1]);
        assert!(
            !KmsExpectation::check(&steady, None, 16_666).pass(),
            "unknown refresh"
        );
        assert!(
            !KmsExpectation::check(&steady, Some(R * 2), 16_666).pass(),
            "wrong refresh"
        );
        assert!(
            !KmsExpectation::check(&steady, Some(R), 33_333).pass(),
            "slow cadence"
        );

        let mut flags = steady.clone();
        flags[1].3 = 0x1 | 0x4;
        assert!(!KmsExpectation::check(&flags, Some(R), 16_666).flags_kms);

        let mut repeated = steady.clone();
        repeated[2].2 = repeated[1].2;
        let check = KmsExpectation::check(&repeated, Some(R), 16_666);
        assert!(!check.seq_increasing && !check.pass());

        let mut skipped = steady;
        skipped[3].2 += 1;
        let check = KmsExpectation::check(&skipped, Some(R), 16_666);
        assert_eq!(check.seq_tv_mismatch, 1);
        assert!(!check.pass());
    }
}

fn run() -> Result<bool, String> {
    let options = options()?;
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("failed to connect to Wayland: {error}"))?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("registry roundtrip failed: {error}"))?;
    let compositor = probe
        .compositor
        .clone()
        .ok_or("wl_compositor unavailable")?;
    let shm = probe.shm.clone().ok_or("wl_shm unavailable")?;
    let wm_base = probe.wm_base.clone().ok_or("xdg_wm_base unavailable")?;
    let presentation = probe.presentation.clone();
    if !options.callbacks_only && presentation.is_none() {
        return Err("wp_presentation is not advertised".into());
    }
    queue
        .roundtrip(&mut probe)
        .map_err(|error| format!("clock roundtrip failed: {error}"))?;

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("cosmix-presentation-probe".into());
    toplevel.set_app_id("dev.cosmix.PresentationProbe".into());
    surface.commit();
    dispatch_until(
        &mut queue,
        &mut probe,
        Instant::now() + Duration::from_secs(10),
        |probe| probe.configured.is_some(),
        "initial configure",
    )?;
    xdg.ack_configure(probe.configured.take().unwrap_or_default());

    let (width, height) = (options.width, options.height);
    let stride = width * 4;
    let buffer_bytes = (stride * height) as usize;
    let name = CString::new("cosmix-presentation-probe").unwrap();
    // SAFETY: name is valid and the successful descriptor is owned below.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(format!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: memfd_create returned a new owned descriptor.
    let backing = unsafe { File::from_raw_fd(raw) };
    let slots = options.burst + 1;
    backing
        .set_len((buffer_bytes * slots) as u64)
        .map_err(|error| error.to_string())?;
    let pool = shm.create_pool(backing.as_fd(), (buffer_bytes * slots) as i32, &qh, ());
    let buffers = (0..slots)
        .map(|index| {
            pool.create_buffer(
                (index * buffer_bytes) as i32,
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Xrgb8888,
                &qh,
                (),
            )
        })
        .collect::<Vec<_>>();

    let commits = options.frames * options.burst;
    probe.feedback = (0..commits).map(|_| FeedbackSlot::default()).collect();
    let mut commit_times = vec![Duration::ZERO; commits];
    let deadline = Instant::now() + options.timeout;
    let started = monotonic_now();
    let mut pixels = vec![0_u8; buffer_bytes];
    let mut commit = 0;
    let mut callbacks = Vec::with_capacity(options.frames);
    for _ in 0..options.frames {
        for step in 0..options.burst {
            // Idle-callback probe: after the first frame, request a callback
            // without reattaching or damaging, so the scene can settle.
            if options.callbacks_only
                && options.burst == 1
                && commit > 0
                && std::env::var("COSMIX_PROBE_IDLE_CALLBACKS").as_deref() == Ok("1")
            {
                surface.frame(&qh, ());
                surface.commit();
                commit += 1;
                continue;
            }
            // Consecutive commits never share a buffer (there is one more
            // buffer than commits per burst).
            let slot = commit % slots;
            let shade = (commit % 256) as u8;
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.copy_from_slice(&[shade, 255 - shade, 0x40, 0xff]);
            }
            backing
                .write_all_at(&pixels, (slot * buffer_bytes) as u64)
                .map_err(|error| error.to_string())?;
            surface.attach(Some(&buffers[slot]), 0, 0);
            surface.damage_buffer(0, 0, width as i32, height as i32);
            if step + 1 == options.burst {
                surface.frame(&qh, ());
            }
            if let Some(presentation) = &presentation
                && !options.callbacks_only
                && asks_feedback(commit, options.burst)
            {
                presentation.feedback(&surface, &qh, commit);
            }
            commit_times[commit] = monotonic_now();
            surface.commit();
            commit += 1;
        }
        probe.frame_done = false;
        dispatch_until(
            &mut queue,
            &mut probe,
            deadline,
            |probe| probe.frame_done,
            "frame callback",
        )?;
        callbacks.push(monotonic_now());
    }
    if options.callbacks_only {
        let mut intervals = callbacks
            .windows(2)
            .map(|pair| pair[1].saturating_sub(pair[0]).as_micros() as u64)
            .collect::<Vec<_>>();
        intervals.sort_unstable();
        let at = |percent: usize| {
            intervals
                .get((intervals.len().saturating_sub(1)) * percent / 100)
                .copied()
                .unwrap_or(0)
        };
        println!(
            "COSMIX_PRESENTATION_PROBE CALLBACKS frames={} callback_p50_us={} \
             callback_p99_us={} callback_min_us={} callback_max_us={} presentation={}",
            options.frames,
            at(50),
            at(99),
            intervals.first().copied().unwrap_or(0),
            intervals.last().copied().unwrap_or(0),
            presentation.is_some(),
        );
        toplevel.destroy();
        return Ok(!intervals.is_empty());
    }
    let committed = monotonic_now();
    let burst = options.burst;
    dispatch_until(
        &mut queue,
        &mut probe,
        deadline,
        |probe| {
            probe
                .feedback
                .iter()
                .enumerate()
                .all(|(commit, slot)| !asks_feedback(commit, burst) || slot.outcome.is_some())
        },
        "feedback resolution",
    )?;
    let finished = monotonic_now();

    // Shown commits: the last of each burst. Superseded: the first of each
    // burst when bursting.
    let mut presented = Vec::new();
    let mut presented_on = Vec::new();
    let mut shown_discarded = 0;
    let mut superseded_discarded = 0;
    // A superseded commit may legitimately be presented if a frame showed
    // it before the rest of its burst arrived. Commit times cannot tell
    // (the next commit follows within microseconds, the host hand-off is
    // milliseconds later), so compare presentation times: presenting it at
    // or after its burst's shown commit would be a lie.
    let presented_at =
        |commit: usize| match probe.feedback.get(commit).and_then(|slot| slot.outcome) {
            Some(Outcome::Presented { time, .. }) => Some(time),
            _ => None,
        };
    let mut superseded_presented_early = 0;
    let mut superseded_presented_late = 0;
    let mut shown_presented = 0;
    let mut tv_before_commit = 0;
    let mut unsynced = 0;
    for (commit, slot) in probe.feedback.iter().enumerate() {
        if !asks_feedback(commit, burst) {
            continue;
        }
        let last = is_last_of_burst(commit, burst);
        match slot.outcome {
            Some(Outcome::Presented {
                time,
                refresh_ns,
                seq,
                flags,
                synced,
            }) => {
                if last {
                    shown_presented += 1;
                } else if presented_at(commit - commit % burst + burst - 1)
                    .is_some_and(|shown_at| time >= shown_at)
                {
                    superseded_presented_late += 1;
                } else {
                    superseded_presented_early += 1;
                }
                if time < commit_times[commit] {
                    tv_before_commit += 1;
                }
                if synced.is_none() {
                    unsynced += 1;
                }
                presented.push((time, refresh_ns, seq, flags));
                presented_on.push(synced);
            }
            Some(Outcome::Discarded) if last => shown_discarded += 1,
            Some(Outcome::Discarded) => superseded_discarded += 1,
            None => {}
        }
    }
    let increasing = presented.windows(2).all(|pair| pair[1].0 > pair[0].0);
    let (in_window, window_lead_ns) = window_check(&presented, started, finished);
    let flags_zero = presented.iter().all(|(.., flags)| *flags == 0);
    let seq_zero = presented.iter().all(|(_, _, seq, _)| *seq == 0);
    let refresh_zero = presented.iter().all(|(_, refresh, ..)| *refresh == 0);
    let intervals = presented
        .windows(2)
        .map(|pair| pair[1].0.saturating_sub(pair[0].0).as_micros() as u64)
        .collect::<Vec<_>>();
    let mut sorted = intervals.clone();
    sorted.sort_unstable();
    let percentile = |p: usize| {
        if sorted.is_empty() {
            0
        } else {
            sorted[(sorted.len() - 1) * p / 100]
        }
    };
    // At least 90% of the shown commits, and never zero.
    let min_presented = options.frames.saturating_mul(9).div_ceil(10).max(1);
    let clock_ok = probe.clock_id == Some(libc::CLOCK_MONOTONIC as u32);
    // At least 90% of the superseded commits are discarded when bursting.
    let min_superseded_discarded = if burst > 1 {
        options.frames.saturating_mul(9).div_ceil(10)
    } else {
        0
    };
    // Which output the frames were presented on (the first synced one).
    let shown_output = presented_on.iter().flatten().next().copied();
    let shown_info = shown_output
        .and_then(|index| probe.output_info.get(index))
        .cloned()
        .unwrap_or_default();
    let output_ok = options.expect_output.as_ref().is_none_or(|expected| {
        presented_on.iter().all(|synced| {
            synced
                .and_then(|index| probe.output_info.get(index))
                .and_then(|info| info.name.as_ref())
                == Some(expected)
        })
    });
    let kms = options.expect_kms.then(|| {
        KmsExpectation::check(
            &presented,
            options.kms_refresh_ns.or_else(|| {
                shown_info
                    .refresh_mhz
                    .and_then(|mhz| u32::try_from(1_000_000_000_000_u64 / u64::from(mhz)).ok())
            }),
            percentile(50),
        )
    });
    let contract_ok = match &kms {
        None => flags_zero && seq_zero && refresh_zero,
        Some(kms) => kms.pass(),
    };
    let pass = shown_presented >= min_presented
        && superseded_presented_late == 0
        && superseded_discarded >= min_superseded_discarded
        && tv_before_commit == 0
        && unsynced == 0
        && !probe.outputs.is_empty()
        && increasing
        && in_window
        && contract_ok
        && output_ok
        && clock_ok;
    if let Some(kms) = &kms {
        println!(
            "COSMIX_PRESENTATION_PROBE_KMS {} output={} output_ok={} {}",
            if kms.pass() { "PASS" } else { "FAIL" },
            shown_info.name.as_deref().unwrap_or("none"),
            output_ok,
            kms.summary(),
        );
    }
    println!(
        "COSMIX_PRESENTATION_PROBE {} frames={} burst={} commits={} presented={} \
         shown_presented={} shown_discarded={} superseded_discarded={} \
         superseded_presented_early={} superseded_presented_late={} \
         min_presented={} clock_id={} outputs={} unsynced={} tv_before_commit={} \
         tv_first_us={} tv_last_us={} window_start_us={} commits_done_us={} \
         window_end_us={} increasing={} in_window={} window_lead_us={} flags_zero={} \
         seq_zero={} refresh_zero={} interval_p50_us={} interval_p99_us={} interval_max_us={}",
        if pass { "PASS" } else { "FAIL" },
        options.frames,
        burst,
        commits,
        presented.len(),
        shown_presented,
        shown_discarded,
        superseded_discarded,
        superseded_presented_early,
        superseded_presented_late,
        min_presented,
        probe
            .clock_id
            .map_or_else(|| "none".to_string(), |clock| clock.to_string()),
        probe.outputs.len(),
        unsynced,
        tv_before_commit,
        presented.first().map_or(0, |entry| entry.0.as_micros()),
        presented.last().map_or(0, |entry| entry.0.as_micros()),
        started.as_micros(),
        committed.as_micros(),
        finished.as_micros(),
        increasing,
        in_window,
        window_lead_ns / 1000,
        flags_zero,
        seq_zero,
        refresh_zero,
        percentile(50),
        percentile(99),
        sorted.last().copied().unwrap_or(0),
    );
    if !options.hold.is_zero() {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let _ = connection.flush();
        std::thread::sleep(options.hold);
    }
    toplevel.destroy();
    Ok(pass)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("COSMIX_PRESENTATION_PROBE failed: {error}");
            ExitCode::FAILURE
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
            }
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "wl_output" => {
                let index = state.outputs.len();
                state
                    .outputs
                    .push(registry.bind(name, version.min(4), qh, index));
                state.output_info.push(OutputInfo::default());
            }
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, 1, qh, ())),
            "wp_presentation" => {
                state.presentation = Some(registry.bind(name, version.min(2), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, usize> for Probe {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        commit: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(slot) = state.feedback.get_mut(*commit) else {
            return;
        };
        let outcome = match event {
            wp_presentation_feedback::Event::SyncOutput { output } => {
                if let Some(index) = state
                    .outputs
                    .iter()
                    .position(|bound| bound.id() == output.id())
                {
                    slot.synced = Some(index);
                }
                return;
            }
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                seq_hi,
                seq_lo,
                flags,
            } => Outcome::Presented {
                time: Duration::new((u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo), tv_nsec),
                refresh_ns: refresh,
                seq: (u64::from(seq_hi) << 32) | u64::from(seq_lo),
                flags: match flags {
                    wayland_client::WEnum::Value(flags) => flags.bits(),
                    wayland_client::WEnum::Unknown(bits) => bits,
                },
                synced: slot.synced,
            },
            wp_presentation_feedback::Event::Discarded => Outcome::Discarded,
            _ => return,
        };
        if slot.outcome.is_some() {
            state.failure = Some(format!("feedback for commit {commit} resolved twice"));
        }
        slot.outcome = Some(outcome);
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            if state.configured.is_none() && !state.feedback.is_empty() {
                // Later configures (focus, size hints) are acked immediately;
                // the probe keeps its own buffer size.
                xdg.ack_configure(serial);
            } else {
                state.configured = Some(serial);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

macro_rules! ignore_events {
    ($($interface:ty),+ $(,)?) => {
        $(
            impl Dispatch<$interface, ()> for Probe {
                fn event(
                    _: &mut Self,
                    _: &$interface,
                    _: <$interface as wayland_client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )+
    };
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
);

impl Dispatch<wl_output::WlOutput, usize> for Probe {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.output_info.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => info.name = Some(name),
            wl_output::Event::Mode {
                flags: wayland_client::WEnum::Value(flags),
                refresh,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                info.refresh_mhz = u32::try_from(refresh).ok().filter(|mhz| *mhz > 0);
            }
            _ => {}
        }
    }
}
