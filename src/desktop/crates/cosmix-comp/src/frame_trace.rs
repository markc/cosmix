//! Opt-in, bounded frame diagnostics. No frame-thread I/O or blocking sends.
use std::{
    io::Write,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
};

const LIMIT: u64 = 65_536;
struct Recorder {
    sender: SyncSender<Record>,
    sequence: AtomicU64,
    dropped: AtomicU64,
    // Runtime record ceiling. 0 == unbounded (file-sink capture); otherwise the
    // recorder stops after `limit` records and emits a single `trace_limit`
    // sentinel, so a stderr sink can never be flooded without bound.
    limit: u64,
}
struct Record {
    sequence: u64,
    stage: &'static str,
    subject: u64,
    detail: u64,
    aux: u64,
    tid: u32,
    start_us: u64,
    end_us: u64,
    cpu_us: u64,
    dropped: u64,
}
static RECORDER: OnceLock<Option<Recorder>> = OnceLock::new();

fn clock_us(clock: libc::clockid_t) -> u64 {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: value points to a valid, writable timespec.
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0 {
        return 0;
    }
    (value.tv_sec as u64)
        .saturating_mul(1_000_000)
        .saturating_add(value.tv_nsec as u64 / 1_000)
}

/// Resolve the record ceiling. `COSMIX_FRAME_TRACE_LIMIT` overrides the default
/// (0 == unbounded, for a file sink capturing a long session); an unset or
/// unparsable value keeps the stderr-safe default so the fast path is unchanged.
fn resolve_limit() -> u64 {
    match std::env::var("COSMIX_FRAME_TRACE_LIMIT") {
        Ok(raw) => raw.trim().parse::<u64>().unwrap_or(LIMIT),
        Err(_) => LIMIT,
    }
}

/// A line sink for the trace thread: a file when `COSMIX_FRAME_TRACE_FILE` names
/// a writable path, else stderr. The file is truncated on open so each capture
/// starts clean, and a path that cannot be opened falls back to stderr rather
/// than losing the trace.
enum Sink {
    File(std::fs::File),
    Stderr(std::io::Stderr),
}

impl Sink {
    fn open() -> Self {
        if let Ok(path) = std::env::var("COSMIX_FRAME_TRACE_FILE")
            && !path.is_empty()
        {
            match std::fs::File::create(&path) {
                Ok(file) => return Sink::File(file),
                Err(error) => {
                    eprintln!("FRAME_TRACE sink open failed path={path} error={error}");
                }
            }
        }
        Sink::Stderr(std::io::stderr())
    }

    fn write_line(&mut self, line: &str) {
        match self {
            Sink::File(file) => {
                let _ = file.write_all(line.as_bytes());
            }
            Sink::Stderr(stderr) => {
                let _ = write!(stderr.lock(), "{line}");
            }
        }
    }
}

fn recorder() -> Option<&'static Recorder> {
    RECORDER.get_or_init(|| {
        if std::env::var("COSMIX_FRAME_TRACE").as_deref() != Ok("1") { return None; }
        let limit = resolve_limit();
        // A file sink can absorb a long capture, so it gets a deeper backlog;
        // stderr keeps the tight channel that never lets tracing dominate a run.
        let file_sink = std::env::var("COSMIX_FRAME_TRACE_FILE").map(|p| !p.is_empty()).unwrap_or(false);
        let depth = if file_sink { 65_536 } else { 4096 };
        let (sender, receiver) = sync_channel::<Record>(depth);
        std::thread::Builder::new().name("frame-trace".into()).spawn(move || {
            let mut sink = Sink::open();
            for r in receiver {
                sink.write_line(&format!(
                    "FRAME_TRACE component=comp pid={} sequence={} stage={} subject={} detail={} aux={} tid={} start_us={} end_us={} duration_us={} cpu_us={} dropped={} limit={}\n",
                    std::process::id(), r.sequence, r.stage, r.subject, r.detail, r.aux, r.tid, r.start_us,
                    r.end_us, r.end_us.saturating_sub(r.start_us), r.cpu_us, r.dropped, limit));
            }
        }).ok()?;
        Some(Recorder { sender, sequence: AtomicU64::new(0), dropped: AtomicU64::new(0), limit })
    }).as_ref()
}

pub(crate) struct Span(Option<(&'static Recorder, Record, u64)>);

/// Also usable on the protocol thread before render observer installation.
pub(crate) fn enabled() -> bool {
    recorder().is_some()
}

fn thread_id() -> u32 {
    // SAFETY: gettid has no arguments or pointers and cannot change state.
    unsafe { libc::gettid() as u32 }
}

/// A protocol observation, not a duration or proof of client receipt. Resolve
/// identities only when tracing is enabled and still within the record budget.
pub(crate) fn event(stage: &'static str, fields: impl FnOnce() -> (u64, u64, u64)) {
    if let Some(recorder) = recorder() {
        event_to(recorder, stage, fields);
    }
}

fn event_to(recorder: &Recorder, stage: &'static str, fields: impl FnOnce() -> (u64, u64, u64)) {
    let sequence = recorder.sequence.fetch_add(1, Ordering::Relaxed);
    let bounded = recorder.limit != 0;
    if bounded && sequence > recorder.limit {
        return;
    }
    let at_limit = bounded && sequence == recorder.limit;
    let (subject, detail, aux) = if at_limit { (0, 0, 0) } else { fields() };
    let now = clock_us(libc::CLOCK_MONOTONIC);
    send_record(
        recorder,
        Record {
            sequence,
            stage: if at_limit { "trace_limit" } else { stage },
            subject,
            detail,
            aux,
            tid: thread_id(),
            start_us: now,
            end_us: now,
            cpu_us: 0,
            dropped: 0,
        },
    );
}

fn send_record(recorder: &Recorder, mut record: Record) {
    record.dropped = recorder.dropped.load(Ordering::Relaxed);
    if recorder.sender.try_send(record).is_err() {
        recorder.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn span(stage: &'static str, subject: u64) -> Span {
    span_with_detail(stage, subject, 0)
}

fn span_with_detail(stage: &'static str, subject: u64, detail: u64) -> Span {
    let Some(recorder) = recorder() else {
        return Span(None);
    };
    let sequence = recorder.sequence.fetch_add(1, Ordering::Relaxed);
    let bounded = recorder.limit != 0;
    if bounded && sequence > recorder.limit {
        return Span(None);
    }
    let stage = if bounded && sequence == recorder.limit {
        "trace_limit"
    } else {
        stage
    };
    Span(Some((
        recorder,
        Record {
            sequence,
            stage,
            subject,
            detail,
            aux: 0,
            tid: thread_id(),
            start_us: clock_us(libc::CLOCK_MONOTONIC),
            end_us: 0,
            cpu_us: 0,
            dropped: 0,
        },
        clock_us(libc::CLOCK_THREAD_CPUTIME_ID),
    )))
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some((recorder, mut record, cpu_start)) = self.0.take() else {
            return;
        };
        record.end_us = clock_us(libc::CLOCK_MONOTONIC);
        record.cpu_us = clock_us(libc::CLOCK_THREAD_CPUTIME_ID).saturating_sub(cpu_start);
        send_record(recorder, record);
    }
}

// These markers measure schedule-boundary intervals, including scheduler
// overhead. CPU belongs to the marker/caller thread, not parallel render tasks.
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
#[derive(Default)]
struct RenderPhases {
    active: Option<(&'static str, Span)>,
    _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
impl RenderPhases {
    fn start(&mut self, stage: &'static str) {
        self.active.take();
        self.active = Some((stage, span(stage, 0)));
    }
}

#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn graph_render_begin(mut phases: bevy::prelude::NonSendMut<RenderPhases>) {
    phases.start("comp_graph_render");
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn graph_submit_begin(mut phases: bevy::prelude::NonSendMut<RenderPhases>) {
    phases.start("comp_graph_submit");
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn graph_submit_end(mut phases: bevy::prelude::NonSendMut<RenderPhases>) {
    phases.active.take();
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn finalize_begin(mut phases: bevy::prelude::NonSendMut<RenderPhases>) {
    phases.start("comp_render_finalize");
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn finalize_end(mut phases: bevy::prelude::NonSendMut<RenderPhases>) {
    phases.active.take();
}

#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn install_graph_markers(render: &mut bevy::app::SubApp) {
    use bevy::{
        prelude::*,
        render::renderer::{RenderGraph, RenderGraphSystems},
    };
    render.world_mut().insert_non_send(RenderPhases::default());
    render.add_systems(
        RenderGraph,
        (
            graph_render_begin
                .after(RenderGraphSystems::Begin)
                .before(RenderGraphSystems::Render),
            graph_submit_begin
                .after(RenderGraphSystems::Render)
                .before(RenderGraphSystems::Submit),
            graph_submit_end
                .after(RenderGraphSystems::Submit)
                .before(RenderGraphSystems::Finish),
            finalize_begin.after(RenderGraphSystems::Finish),
        ),
    );
}

/// Opt-in only, installed after Bevy finishes configuring its renderer. The
/// original render_system and RenderGraph executors remain untouched.
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
pub(crate) fn install_render_phases(app: &mut bevy::prelude::App) {
    use bevy::{
        prelude::*,
        render::{Render, RenderApp, RenderSystems, renderer::render_system},
    };
    let Some(render) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    if recorder().is_none() {
        return;
    }
    if render.world().contains_non_send::<RenderPhases>() {
        return;
    }
    if wgpu::diagnostics::install(wgpu_event).is_err() {
        tracing::warn!("wgpu diagnostic observer already installed; keeping its owner");
    }
    if !cosmix_wgpu_dmabuf::diagnostics::install(|subject, detail, aux| {
        event("comp_dmabuf_acquire_state", || (subject, detail, aux));
    }) {
        tracing::warn!("DMA-BUF diagnostic observer already installed; keeping its owner");
    }
    if !wgpu::hal::diagnostics::install_allocations(|stage, subject, detail, aux| {
        event(stage, || (subject, detail, aux));
    }) {
        tracing::warn!("wgpu allocation diagnostic observer already installed; keeping its owner");
    }
    install_graph_markers(render);
    render.add_systems(
        Render,
        finalize_end
            .after(render_system)
            .in_set(RenderSystems::Render),
    );
}

// Bound callback nesting without allocating for every public GPU API call.
// Suppressed nested calls still balance their ends and never pop an outer span.
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
#[derive(Default)]
struct GpuCalls {
    stack: Vec<(wgpu::diagnostics::Operation, u64, u64, Span)>,
    suppressed: usize,
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
impl GpuCalls {
    fn fields(&self, event: wgpu::diagnostics::Event) -> (u64, u64) {
        use wgpu::diagnostics::Operation;
        if matches!(
            event.operation,
            Operation::QueueSubmitInner
                | Operation::QueueDeferredActions
                | Operation::QueueFenceLock
                | Operation::QueueCommandPrep
                | Operation::QueuePendingWrites
                | Operation::QueueHalSubmit
                | Operation::QueueHalBookkeeping
                | Operation::QueueVkSubmit
                | Operation::QueueMaintenance
        ) {
            self.stack
                .iter()
                .rev()
                .find_map(|(operation, subject, detail, _)| {
                    (*operation == Operation::QueueSubmit).then_some((*subject, *detail))
                })
                .unwrap_or((event.subject, event.detail))
        } else {
            (event.subject, event.detail)
        }
    }

    fn event(&mut self, event: wgpu::diagnostics::Event) {
        use wgpu::diagnostics::{Operation, Phase};
        match event.phase {
            Phase::Begin => {
                if self.suppressed != 0 || self.stack.len() >= 16 {
                    self.suppressed = self.suppressed.saturating_add(1);
                    return;
                }
                let stage = match event.operation {
                    Operation::QueueSubmit => "comp_wgpu_queue_submit",
                    Operation::QueueSubmitInner => "comp_wgpu_queue_submit_inner",
                    Operation::QueueDeferredActions => "comp_wgpu_queue_deferred_actions",
                    Operation::QueueFenceLock => "comp_wgpu_submit_fence_lock",
                    Operation::QueueCommandPrep => "comp_wgpu_submit_command_prep",
                    Operation::QueuePendingWrites => "comp_wgpu_submit_pending_writes",
                    Operation::QueueHalSubmit => "comp_wgpu_submit_hal",
                    Operation::QueueHalBookkeeping => "comp_wgpu_submit_hal_bookkeeping",
                    Operation::QueueVkSubmit => "comp_wgpu_submit_vk",
                    Operation::QueueMaintenance => "comp_wgpu_submit_maintenance",
                    Operation::DevicePoll => "comp_wgpu_device_poll",
                    Operation::SurfaceConfigure => "comp_wgpu_surface_configure",
                    Operation::SurfaceAcquire => "comp_wgpu_surface_acquire",
                    Operation::SurfacePresent => "comp_wgpu_surface_present",
                };
                // Nested queue phases inherit the exact public submit's site
                // and origin. Keep event.subject separately for balanced ends.
                let (subject, detail) = self.fields(event);
                self.stack.push((
                    event.operation,
                    event.subject,
                    detail,
                    span_with_detail(stage, subject, detail),
                ));
            }
            Phase::End => {
                if self.suppressed != 0 {
                    self.suppressed -= 1;
                } else if self.stack.last().is_some_and(|(operation, subject, _, _)| {
                    *operation == event.operation && *subject == event.subject
                }) {
                    self.stack.pop();
                }
            }
        }
    }
}
#[cfg_attr(not(feature = "kms-live"), allow(dead_code))]
fn wgpu_event(event: wgpu::diagnostics::Event) {
    std::thread_local! {
        static CALLS: std::cell::RefCell<GpuCalls> = std::cell::RefCell::new(GpuCalls::default());
    }
    let _ = CALLS.try_with(|calls| {
        if let Ok(mut calls) = calls.try_borrow_mut() {
            calls.event(event);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        app::SubApp,
        prelude::*,
        render::renderer::{RenderGraph, RenderGraphSystems},
    };

    #[test]
    fn protocol_events_are_bounded_and_keep_identity_and_thread() {
        let (sender, receiver) = sync_channel(1);
        let recorder = Recorder {
            sender,
            sequence: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            limit: LIMIT,
        };
        event_to(&recorder, "test_release", || (17, 23, 2));
        // A full queue drops instead of blocking the protocol thread.
        event_to(&recorder, "test_full", || (0, 0, 0));
        assert_eq!(recorder.dropped.load(Ordering::Relaxed), 1);
        let record = receiver.try_recv().unwrap();
        assert_eq!((record.subject, record.detail, record.aux), (17, 23, 2));
        assert_eq!(record.tid, thread_id());
        assert_eq!(record.start_us, record.end_us);
        assert!(record.start_us > 0);
        assert_eq!(record.cpu_us, 0);
        recorder.sequence.store(LIMIT, Ordering::Relaxed);
        event_to(&recorder, "test_limit", || {
            panic!("limit must not resolve identities")
        });
        assert_eq!(receiver.try_recv().unwrap().stage, "trace_limit");
        event_to(&recorder, "test_past_limit", || {
            panic!("exhausted recorder must not resolve identities")
        });
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn unbounded_limit_keeps_recording_past_the_default_cap() {
        // A file-sink capture (limit == 0) must never stop or emit the sentinel,
        // even after passing the bounded default — otherwise a long mouse-move
        // trace would silently truncate exactly where the interesting data is.
        let (sender, receiver) = sync_channel(4);
        let recorder = Recorder {
            sender,
            sequence: AtomicU64::new(LIMIT),
            dropped: AtomicU64::new(0),
            limit: 0,
        };
        event_to(&recorder, "past_cap", || (9, 8, 7));
        let record = receiver.try_recv().unwrap();
        assert_eq!(record.sequence, LIMIT);
        assert_eq!(record.stage, "past_cap");
        assert_eq!((record.subject, record.detail, record.aux), (9, 8, 7));
    }

    #[test]
    fn installer_without_render_app_leaves_application_untouched() {
        let mut app = App::new();
        let original_schedule = app.main().update_schedule;
        // This returns before recorder/observer installation, even if the test
        // process opted into tracing; it cannot affect other GPU tests.
        install_render_phases(&mut app);
        assert_eq!(app.main().update_schedule, original_schedule);
        assert!(app.get_sub_app(bevy::render::RenderApp).is_none());
        assert!(!app.world().contains_non_send::<RenderPhases>());
    }

    #[test]
    fn gpu_callback_overflow_balances_without_losing_outer_calls() {
        use wgpu::diagnostics::{Event, Operation, Phase};
        let mut calls = GpuCalls::default();
        for subject in 0..20 {
            calls.event(Event {
                operation: Operation::SurfacePresent,
                phase: Phase::Begin,
                subject,
                detail: 0,
            });
        }
        assert_eq!(calls.stack.len(), 16);
        assert_eq!(calls.suppressed, 4);
        for subject in (0..20).rev() {
            calls.event(Event {
                operation: Operation::SurfacePresent,
                phase: Phase::End,
                subject,
                detail: 0,
            });
        }
        assert!(calls.stack.is_empty());
        assert_eq!(calls.suppressed, 0);
    }

    #[test]
    fn core_phases_inherit_submit_site_and_origin_without_relabelling_polls() {
        use wgpu::diagnostics::{Event, Operation, Phase};
        let mut calls = GpuCalls::default();
        let submit = Event {
            operation: Operation::QueueSubmit,
            phase: Phase::Begin,
            subject: 123,
            detail: 1,
        };
        calls.event(submit);
        for operation in [
            Operation::QueueSubmitInner,
            Operation::QueueFenceLock,
            Operation::QueueCommandPrep,
            Operation::QueuePendingWrites,
            Operation::QueueHalSubmit,
            Operation::QueueHalBookkeeping,
            Operation::QueueVkSubmit,
            Operation::QueueMaintenance,
            Operation::QueueDeferredActions,
        ] {
            let begin = Event {
                operation,
                phase: Phase::Begin,
                subject: 0,
                detail: 0,
            };
            assert_eq!(calls.fields(begin), (123, 1));
            calls.event(begin);
            calls.event(Event {
                phase: Phase::End,
                ..begin
            });
        }
        let poll = Event {
            operation: Operation::DevicePoll,
            phase: Phase::Begin,
            subject: 1,
            detail: 0,
        };
        assert_eq!(calls.fields(poll), (1, 0));
        let nested = Event {
            subject: 456,
            detail: 3,
            ..submit
        };
        calls.event(nested);
        let core = Event {
            operation: Operation::QueueHalSubmit,
            subject: 0,
            detail: 0,
            ..submit
        };
        assert_eq!(calls.fields(core), (456, 3));
        calls.event(Event {
            phase: Phase::End,
            ..nested
        });
        assert_eq!(calls.fields(core), (123, 1));
        calls.event(Event {
            phase: Phase::End,
            ..submit
        });
        assert!(calls.stack.is_empty());
    }

    #[test]
    fn graph_markers_bracket_original_work_and_leave_finalisation_open() {
        #[derive(Resource, Default)]
        struct Seen(Vec<&'static str>);
        let mut render = SubApp::new();
        render
            .add_schedule(RenderGraph::base_schedule())
            .init_resource::<Seen>();
        install_graph_markers(&mut render);
        render.add_systems(
            RenderGraph,
            (
                (|phases: NonSend<RenderPhases>, mut seen: ResMut<Seen>| {
                    assert_eq!(
                        phases.active.as_ref().map(|p| p.0),
                        Some("comp_graph_render")
                    );
                    seen.0.push("render");
                })
                .in_set(RenderGraphSystems::Render),
                (|phases: NonSend<RenderPhases>, mut seen: ResMut<Seen>| {
                    assert_eq!(
                        phases.active.as_ref().map(|p| p.0),
                        Some("comp_graph_submit")
                    );
                    seen.0.push("submit");
                })
                .in_set(RenderGraphSystems::Submit),
                (|phases: NonSend<RenderPhases>, mut seen: ResMut<Seen>| {
                    assert!(phases.active.is_none());
                    seen.0.push("finish");
                })
                .in_set(RenderGraphSystems::Finish),
            ),
        );
        for _ in 0..2 {
            render.world_mut().run_schedule(RenderGraph);
            assert_eq!(
                render
                    .world()
                    .non_send::<RenderPhases>()
                    .active
                    .as_ref()
                    .map(|p| p.0),
                Some("comp_render_finalize")
            );
            render
                .world_mut()
                .non_send_mut::<RenderPhases>()
                .active
                .take();
        }
        assert_eq!(
            render.world().resource::<Seen>().0,
            ["render", "submit", "finish", "render", "submit", "finish"]
        );
    }
}
