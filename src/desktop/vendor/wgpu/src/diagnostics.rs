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
    /// Process-local surface identity, or zero for queue submissions.
    /// Reconfiguration preserves it; a new surface gets a new identity.
    pub subject: u64,
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
    OBSERVER.0.set(observer).map_err(|_| AlreadyInstalled)
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
}
