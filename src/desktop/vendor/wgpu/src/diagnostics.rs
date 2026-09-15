//! Optional Cosmix timing hooks at public GPU API boundaries.
//!
//! No observer means no clock reads, logging, allocation, or identity assignment.
//! Observers run synchronously and must not block or call back into wgpu. Timing
//! and bounded recording belong to the consumer. Panics are caught on unwind
//! builds; a panic-abort build cannot recover from a faulty observer.

use alloc::rc::Rc;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicUsize, Ordering},
};
use std::sync::OnceLock;

/// Public API operation being observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Queue submission, including iterator drain and deferred actions.
    QueueSubmit,
    /// Backend submission, including command-buffer iterator drain.
    QueueSubmitInner,
    /// Deferred actions executed after backend submission returns.
    QueueDeferredActions,
    /// Core submit: device fence write-lock acquisition.
    QueueFenceLock,
    /// Core submit: command preparation and resource initialisation.
    QueueCommandPrep,
    /// Core submit: pending-write preparation.
    QueuePendingWrites,
    /// Core submit: HAL submission.
    QueueHalSubmit,
    /// Vulkan HAL semaphore/fence bookkeeping before the driver call.
    QueueHalBookkeeping,
    /// Raw Vulkan loader/driver queue submission only.
    QueueVkSubmit,
    /// Core submit: tracking and maintenance.
    QueueMaintenance,
    /// Explicit device poll; subject is 0 for Poll, 1 for Wait.
    DevicePoll,
    /// Surface configuration.
    SurfaceConfigure,
    /// Acquisition of the next surface texture.
    SurfaceAcquire,
    /// Presentation of an acquired texture.
    SurfacePresent,
}

/// Boundary of an observed operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Entering the public API call.
    Begin,
    /// Leaving the call, including unwinding; this is not a success receipt.
    End,
}

/// Clock-free event delivered on the calling thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// Operation being observed.
    pub operation: Operation,
    /// Entry or exit boundary.
    pub phase: Phase,
    /// Process-local surface identity, or a hashed source location for submits.
    /// Reconfiguration preserves it; a new surface gets a new identity.
    pub subject: u64,
    /// Submit origin ID (see [`SubmitOrigin`]); zero for other operations.
    pub detail: u64,
}

/// Cosmix downstream submit origins, encoded directly in trace `detail`.
/// Unknown/new callers retain a hashed call-site identity in `subject`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u64)]
pub enum SubmitOrigin {
    /// Other caller; distinguish sites using `subject`.
    #[default]
    Other = 0,
    /// Sampled DMA-BUF ownership acquisition.
    Acquire = 1,
    /// Bevy render graph command flush.
    Graph = 2,
    /// DMA-BUF FOREIGN ownership release.
    Release = 3,
    /// Retirement worker's empty completion marker.
    Retirement = 4,
    /// Cursor composition.
    Cursor = 5,
    /// Scanout completion's empty marker.
    EmptyMarker = 6,
    /// Capture copy or fallback blit.
    Capture = 7,
    /// Opt-in diagnostic readback.
    Probe = 8,
    /// Unwritten output clear.
    Clear = 9,
    /// Resource normalisation preceding ownership release.
    ReleaseNormalise = 10,
    /// Bevy post-graph screenshot/readback submission.
    BevyFinalize = 11,
    /// Other Bevy upload/storage submission.
    BevyUpload = 12,
}

std::thread_local! {
    static SUBMIT_ORIGIN: core::cell::Cell<SubmitOrigin> = const { core::cell::Cell::new(SubmitOrigin::Other) };
}

/// Same-thread, nesting-safe submit attribution. Disabled diagnostics do not
/// access thread-local storage. This guard never changes submission ordering.
pub struct SubmitOriginGuard {
    previous: Option<SubmitOrigin>,
    _same_thread: PhantomData<Rc<()>>,
}

impl Drop for SubmitOriginGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous {
            let _ = SUBMIT_ORIGIN.try_with(|origin| origin.set(previous));
        }
    }
}

/// Attribute submissions in this scope; restore the previous origin on unwind.
pub fn submit_origin(origin: SubmitOrigin) -> SubmitOriginGuard {
    SubmitOriginGuard {
        previous: OBSERVER
            .0
            .get()
            .and_then(|_| SUBMIT_ORIGIN.try_with(|slot| slot.replace(origin)).ok()),
        _same_thread: PhantomData,
    }
}

/// Supply a helper's default origin without hiding a more specific caller.
pub fn submit_origin_if_unset(origin: SubmitOrigin) -> SubmitOriginGuard {
    SubmitOriginGuard {
        previous: OBSERVER.0.get().and_then(|_| {
            SUBMIT_ORIGIN
                .try_with(|slot| {
                    let previous = slot.get();
                    if previous == SubmitOrigin::Other {
                        slot.set(origin);
                    }
                    previous
                })
                .ok()
        }),
        _same_thread: PhantomData,
    }
}

fn caller_origin(file: &str) -> SubmitOrigin {
    // Bevy is not vendored. Classify its actual submitting thread/call site,
    // not a schedule marker's thread. New sites still get their own subject.
    if file.contains("bevy_core_pipeline") && file.ends_with("/schedule.rs") {
        SubmitOrigin::Graph
    } else if file.contains("bevy_render") {
        if file.ends_with("/renderer/render_context.rs") {
            SubmitOrigin::Graph
        } else if file.ends_with("/renderer/mod.rs") {
            SubmitOrigin::BevyFinalize
        } else {
            SubmitOrigin::BevyUpload
        }
    } else {
        SubmitOrigin::Other
    }
}

pub(crate) fn begin_submit(caller: &'static core::panic::Location<'static>) -> Guard {
    let observer = OBSERVER.0.get().copied();
    let (subject, detail) = if observer.is_some() {
        // FNV-1a over the file/line/column: allocation-free and no source paths
        // emitted into the trace. This also identifies callers without a tag.
        let mut hash = 0xcbf29ce484222325_u64;
        for byte in caller
            .file()
            .bytes()
            .chain(caller.line().to_le_bytes())
            .chain(caller.column().to_le_bytes())
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        let origin = SUBMIT_ORIGIN
            .try_with(|slot| slot.get())
            .unwrap_or_default();
        let origin = if origin == SubmitOrigin::Other {
            caller_origin(caller.file())
        } else {
            origin
        };
        (hash, origin as u64)
    } else {
        (0, 0)
    };
    let event = Event {
        operation: Operation::QueueSubmit,
        phase: Phase::Begin,
        subject,
        detail,
    };
    if let Some(observer) = observer {
        notify(observer, event);
    }
    Guard {
        observer,
        event,
        _same_thread: PhantomData,
    }
}

/// An observer has already been installed for this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlreadyInstalled;

#[derive(Default)]
struct Observer(OnceLock<fn(Event)>);
static OBSERVER: Observer = Observer(OnceLock::new());

/// Install the single process-lifetime observer. Repeated installation fails
/// without replacing the original callback or disturbing in-flight calls.
pub fn install(observer: fn(Event)) -> Result<(), AlreadyInstalled> {
    OBSERVER.0.set(observer).map_err(|_| AlreadyInstalled)?;
    #[cfg(wgpu_core)]
    let _ = crate::wgc::diagnostics::install(core_event);
    #[cfg(wgpu_core)]
    let _ = crate::hal::diagnostics::install(hal_event);
    Ok(())
}

#[cfg(wgpu_core)]
fn hal_event(event: crate::hal::diagnostics::Event) {
    use crate::hal::diagnostics::Operation as Hal;
    if let Some(observer) = OBSERVER.0.get().copied() {
        notify(
            observer,
            Event {
                operation: match event.operation {
                    Hal::Bookkeeping => Operation::QueueHalBookkeeping,
                    Hal::VulkanSubmit => Operation::QueueVkSubmit,
                },
                phase: if event.begin {
                    Phase::Begin
                } else {
                    Phase::End
                },
                subject: 0,
                detail: 0,
            },
        );
    }
}

#[cfg(wgpu_core)]
fn core_event(event: crate::wgc::diagnostics::Event) {
    use crate::wgc::diagnostics::Operation as Core;
    let Some(observer) = OBSERVER.0.get().copied() else {
        return;
    };
    notify(
        observer,
        Event {
            operation: match event.operation {
                Core::FenceLock => Operation::QueueFenceLock,
                Core::CommandPrep => Operation::QueueCommandPrep,
                Core::PendingWrites => Operation::QueuePendingWrites,
                Core::HalSubmit => Operation::QueueHalSubmit,
                Core::Maintenance => Operation::QueueMaintenance,
            },
            phase: if event.begin {
                Phase::Begin
            } else {
                Phase::End
            },
            subject: 0,
            detail: 0,
        },
    );
}

fn notify(observer: fn(Event), event: Event) {
    let _ = std::panic::catch_unwind(|| observer(event));
}

pub(crate) struct Guard {
    observer: Option<fn(Event)>,
    event: Event,
    _same_thread: PhantomData<Rc<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(observer) = self.observer {
            notify(
                observer,
                Event {
                    phase: Phase::End,
                    ..self.event
                },
            );
        }
    }
}
impl Observer {
    fn begin(&self, operation: Operation, subject: impl FnOnce() -> u64) -> Guard {
        let observer = self.0.get().copied();
        let event = Event {
            operation,
            phase: Phase::Begin,
            subject: if observer.is_some() { subject() } else { 0 },
            detail: 0,
        };
        if let Some(observer) = observer {
            notify(observer, event);
        }
        Guard {
            observer,
            event,
            _same_thread: PhantomData,
        }
    }
}

pub(crate) fn begin(operation: Operation, subject: impl FnOnce() -> u64) -> Guard {
    OBSERVER.begin(operation, subject)
}

/// Identity is allocated lazily only while diagnostics are installed.
#[derive(Debug, Default)]
pub(crate) struct SurfaceIdentity(OnceLock<u64>);
impl SurfaceIdentity {
    pub(crate) fn get(&self) -> u64 {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        *self.0.get_or_init(|| {
            NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .unwrap_or(0) as u64
        })
    }
    pub(crate) fn assigned(&self) -> u64 {
        self.0.get().copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    std::thread_local! { static EVENTS: RefCell<Vec<Event>> = const { RefCell::new(Vec::new()) }; }
    fn record(event: Event) {
        EVENTS.with(|events| events.borrow_mut().push(event));
    }
    fn other(_: Event) {
        panic!("replacement observer must not be installed");
    }

    #[test]
    fn disabled_observer_does_not_evaluate_identity() {
        let touched = Cell::new(false);
        let observer = Observer::default();
        drop(observer.begin(Operation::QueueSubmit, || {
            touched.set(true);
            9
        }));
        assert!(!touched.get());
    }

    #[test]
    fn boundaries_nest_unwind_and_keep_original_observer() {
        EVENTS.with(|events| events.borrow_mut().clear());
        let observer = Observer::default();
        assert!(observer.0.set(record).is_ok());
        assert!(observer.0.set(other).is_err());
        let _ = std::panic::catch_unwind(|| {
            let _outer = observer.begin(Operation::QueueSubmit, || 0);
            let _inner = observer.begin(Operation::SurfacePresent, || 7);
            panic!("simulated API unwind");
        });
        EVENTS.with(|events| {
            let events = events.borrow();
            assert_eq!(events.len(), 4);
            assert_eq!(events[0].phase, Phase::Begin);
            assert_eq!(events[1].subject, 7);
            assert_eq!(
                events[2],
                Event {
                    phase: Phase::End,
                    ..events[1]
                }
            );
            assert_eq!(
                events[3],
                Event {
                    phase: Phase::End,
                    ..events[0]
                }
            );
        });
    }

    #[test]
    fn observer_panic_does_not_escape_api_guard() {
        let observer = Observer::default();
        observer.0.set(other).unwrap();
        drop(observer.begin(Operation::SurfaceAcquire, || 1));
    }

    #[test]
    fn surface_identity_survives_moves_and_differs_for_new_surfaces() {
        let identity = SurfaceIdentity::default();
        assert_eq!(identity.assigned(), 0);
        let id = identity.get();
        let moved = identity;
        assert_eq!(moved.get(), id);
        assert_eq!(moved.assigned(), id);
        assert_ne!(SurfaceIdentity::default().get(), id);
    }

    #[test]
    fn submit_origins_nest_restore_on_unwind_and_forward_core_phases() {
        // Other observer tests use private Observer instances, so this is the
        // sole process-global installation in this module.
        install(record).expect("this test owns the process-global observer");
        EVENTS.with(|events| events.borrow_mut().clear());
        assert_eq!(
            caller_origin("/registry/bevy_core_pipeline-0.19.1/src/schedule.rs"),
            SubmitOrigin::Graph
        );
        assert_eq!(
            caller_origin("/registry/bevy_render-0.19.1/src/renderer/render_context.rs"),
            SubmitOrigin::Graph
        );
        assert_eq!(
            caller_origin("/registry/bevy_render-0.19.1/src/renderer/mod.rs"),
            SubmitOrigin::BevyFinalize
        );
        assert_eq!(
            caller_origin("/registry/bevy_render-0.19.1/src/texture/gpu_image.rs"),
            SubmitOrigin::BevyUpload
        );

        let site = core::panic::Location::caller();
        let outer = submit_origin(SubmitOrigin::EmptyMarker);
        {
            let _default = submit_origin_if_unset(SubmitOrigin::Retirement);
            let _submit = begin_submit(site);
            #[cfg(wgpu_core)]
            for begin in [true, false] {
                core_event(crate::wgc::diagnostics::Event {
                    operation: crate::wgc::diagnostics::Operation::FenceLock,
                    begin,
                });
            }
        }
        let _ = std::panic::catch_unwind(|| {
            let _nested = submit_origin(SubmitOrigin::Release);
            let _submit = begin_submit(site);
            panic!("exercise origin restoration");
        });
        drop(begin_submit(site));
        drop(outer);
        drop(begin_submit(site));
        EVENTS.with(|events| {
            let events = events.borrow();
            let submits = events
                .iter()
                .filter(|event| event.operation == Operation::QueueSubmit)
                .collect::<Vec<_>>();
            assert_eq!(submits.len(), 8);
            for pair in submits.chunks_exact(2) {
                assert_eq!(pair[0].phase, Phase::Begin);
                assert_eq!(pair[1].phase, Phase::End);
                assert_ne!(pair[0].subject, 0);
                assert_eq!(pair[0].subject, pair[1].subject);
                assert_eq!(pair[0].detail, pair[1].detail);
            }
            assert_eq!(
                submits
                    .iter()
                    .step_by(2)
                    .map(|event| event.detail)
                    .collect::<Vec<_>>(),
                [6, 3, 6, 0]
            );
            #[cfg(wgpu_core)]
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.operation == Operation::QueueFenceLock)
                    .count(),
                2
            );
        });
    }
}
