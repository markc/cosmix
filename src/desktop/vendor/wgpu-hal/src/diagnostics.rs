//! Cosmix downstream: clock-free Vulkan submit boundaries. Keep when refreshing
//! this vendor. No observer means no clocks, allocation or callbacks.

/// Vulkan submission boundary being observed.
#[derive(Clone, Copy, Debug)]
pub enum Operation {
    /// Semaphore/fence bookkeeping before calling the loader/driver.
    Bookkeeping,
    /// The raw Vulkan queue_submit call alone.
    VulkanSubmit,
}

/// Entry/exit notification; exit also covers unwinding.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Boundary being observed.
    pub operation: Operation,
    /// True at entry, false at exit.
    pub begin: bool,
}

#[cfg(vulkan)]
static OBSERVER: std::sync::OnceLock<fn(Event)> = std::sync::OnceLock::new();

/// Install a process-lifetime observer. False if unavailable or already owned.
pub fn install(observer: fn(Event)) -> bool {
    #[cfg(vulkan)]
    {
        OBSERVER.set(observer).is_ok()
    }
    #[cfg(not(vulkan))]
    {
        let _ = observer;
        false
    }
}

#[cfg(vulkan)]
pub(crate) struct Guard {
    observer: Option<fn(Event)>,
    operation: Operation,
    _same_thread: core::marker::PhantomData<alloc::rc::Rc<()>>,
}

#[cfg(vulkan)]
fn notify(observer: fn(Event), operation: Operation, begin: bool) {
    let _ = std::panic::catch_unwind(|| observer(Event { operation, begin }));
}

#[cfg(vulkan)]
pub(crate) fn begin(operation: Operation) -> Guard {
    let observer = OBSERVER.get().copied();
    if let Some(observer) = observer {
        notify(observer, operation, true);
    }
    Guard {
        observer,
        operation,
        _same_thread: core::marker::PhantomData,
    }
}

#[cfg(vulkan)]
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(observer) = self.observer {
            notify(observer, self.operation, false);
        }
    }
}

/// Numeric allocation observation: stage, subject, detail, aux.
pub type AllocationObserver = fn(&'static str, u64, u64, u64);
#[cfg(vulkan)]
static ALLOC_OBSERVER: std::sync::OnceLock<AllocationObserver> = std::sync::OnceLock::new();

/// Install resource and allocator-block observations, independently of submit spans.
pub fn install_allocations(observer: AllocationObserver) -> bool {
    #[cfg(vulkan)]
    {
        if ALLOC_OBSERVER.set(observer).is_err() {
            return false;
        }
        gpu_allocator::vulkan::diagnostics::install(block_event)
    }
    #[cfg(not(vulkan))]
    {
        let _ = observer;
        false
    }
}

#[cfg(vulkan)]
fn block_event(stage: &'static str, subject: u64, detail: u64, aux: u64) {
    if let Some(observer) = ALLOC_OBSERVER.get() {
        let _ = std::panic::catch_unwind(|| observer(stage, subject, detail, aux));
    }
}

#[cfg(vulkan)]
pub(crate) fn allocation(
    allocate: bool,
    kind: u64,
    resource: impl FnOnce() -> u64,
    allocation: &gpu_allocator::vulkan::Allocation,
    name: &str,
    usage: u64,
) {
    use ash::vk::Handle;
    let Some(observer) = ALLOC_OBSERVER.get() else {
        return;
    };
    let resource = resource();
    let _ = std::panic::catch_unwind(|| {
        observer(
            if allocate {
                "comp_vk_alloc"
            } else {
                "comp_vk_free"
            },
            resource,
            allocation.size(),
            kind,
        );
        // Join a resource lifetime to its backing block and byte offset. Handles
        // may be reused: consumers must process alloc/free records in time order.
        observer(
            "comp_vk_alloc_memory",
            resource,
            unsafe { allocation.memory().as_raw() },
            allocation.offset(),
        );
        if allocate {
            let hash = label_hash(name);
            observer("comp_vk_alloc_meta", resource, hash, usage);
            observer(
                "comp_vk_alloc_properties",
                resource,
                allocation.memory_properties().as_raw() as u64,
                u64::from(allocation.is_dedicated()),
            );
            // Numeric, reversible label dictionary. No strings or I/O on the
            // frame recorder; each little-endian chunk has its byte offset.
            observer("comp_vk_label_len", hash, name.len() as u64, 0);
            for (index, chunk) in name.as_bytes().chunks(8).enumerate() {
                let mut bytes = [0; 8];
                bytes[..chunk.len()].copy_from_slice(chunk);
                observer(
                    "comp_vk_label",
                    hash,
                    (index * 8) as u64,
                    u64::from_le_bytes(bytes),
                );
            }
        }
    });
}

#[cfg(vulkan)]
fn label_hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(all(test, vulkan))]
mod allocation_tests {
    #[test]
    fn disabled_allocations_do_not_resolve_resource_identity() {
        super::allocation(
            true,
            1,
            || panic!("disabled identity"),
            &Default::default(),
            "test",
            0,
        );
    }

    #[test]
    fn label_hash_has_stable_external_encoding() {
        assert_eq!(super::label_hash(""), 0xcbf29ce484222325);
        assert_eq!(super::label_hash("hello"), 0xa430d84680aabd0b);
    }
}
