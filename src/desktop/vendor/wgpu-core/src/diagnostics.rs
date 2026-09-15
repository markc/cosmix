//! Cosmix downstream submit attribution. Clock-free, optional, synchronous hooks.
//! Keep this patch when refreshing the vendor sources. No observer means no
//! clocks, allocations, callbacks or changes to queue/lock ordering.

/// Internal submit phase being observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Acquisition of the device fence write lock only.
    FenceLock,
    /// Command preparation, validation and resource initialisation.
    CommandPrep,
    /// Pending-write locking, transitions and preparation.
    PendingWrites,
    /// HAL queue submission only.
    HalSubmit,
    /// Submission tracking and device maintenance.
    Maintenance,
}

/// Clock-free boundary; `begin == false` also covers error/unwind exits.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Phase being observed.
    pub operation: Operation,
    /// Whether this is the entry boundary.
    pub begin: bool,
}

#[cfg(feature = "std")]
static OBSERVER: std::sync::OnceLock<fn(Event)> = std::sync::OnceLock::new();

/// Install a process-lifetime observer without replacing an existing one.
#[cfg(feature = "std")]
pub fn install(observer: fn(Event)) -> bool {
    OBSERVER.set(observer).is_ok()
}

pub(crate) struct Guard {
    #[cfg(feature = "std")]
    observer: Option<fn(Event)>,
    operation: Operation,
    _same_thread: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

#[cfg(feature = "std")]
fn notify(observer: fn(Event), operation: Operation, begin: bool) {
    let _ = std::panic::catch_unwind(|| observer(Event { operation, begin }));
}

pub(crate) fn begin(operation: Operation) -> Guard {
    #[cfg(feature = "std")]
    let observer = OBSERVER.get().copied();
    #[cfg(feature = "std")]
    if let Some(observer) = observer {
        notify(observer, operation, true);
    }
    Guard {
        #[cfg(feature = "std")]
        observer,
        operation,
        _same_thread: core::marker::PhantomData,
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "std")]
        if let Some(observer) = self.observer {
            notify(observer, self.operation, false);
        }
        #[cfg(not(feature = "std"))]
        let _ = self.operation;
    }
}
