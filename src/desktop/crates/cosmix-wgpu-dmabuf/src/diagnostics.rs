//! Opt-in acquisition observations. Never used to gate or change rendering.
//!
//! ENABLED-path cost lives HERE (not in wgpu-hal): capture() dup()s one fd
//! per plane per import, and record() poll()s each at acquisition — inside
//! the import-registry lock. Details and the lock-hoist deferral rationale:
//! vendor/wgpu-hal/README.cosmix.md.
use std::{
    os::fd::{AsRawFd, OwnedFd},
    sync::{Once, OnceLock},
};

use crate::{DmabufBufferId, DmabufDescriptor};

static OBSERVER: OnceLock<fn(u64, u64, u64)> = OnceLock::new();
static CLONE_WARNING: Once = Once::new();

/// Install a process-lifetime observer (subject, detail, aux).
/// Subject is the compositor's opaque buffer identity; aux is the number of
/// distinct backings in the acquisition submission. Detail encodes BOTH facts:
/// 0 implicit/ready, 1 implicit/unready, 2 explicit/ready, 3 explicit/unready,
/// 4 implicit/probe-error, 5 explicit/probe-error. Explicit refers only to
/// acquire-point presence; implicit readiness is not an explicit-fence proof.
pub fn install(observer: fn(u64, u64, u64)) -> bool {
    OBSERVER.set(observer).is_ok()
}

/// No clocks, allocations, FD duplication or polling when disabled.
pub fn enabled() -> bool {
    OBSERVER.get().is_some()
}

pub(crate) struct AcquireState {
    buffer_id: DmabufBufferId,
    explicit: bool,
    planes: Option<Vec<OwnedFd>>,
}

impl AcquireState {
    pub(crate) fn capture(
        buffer_id: DmabufBufferId,
        descriptor: &DmabufDescriptor,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        Some(Self {
            buffer_id,
            explicit: descriptor.explicit_acquire,
            // Keep independent FDs because Vulkan import consumes its copies.
            // Failure is diagnostic data, never an import failure.
            planes: descriptor
                .planes
                .iter()
                .map(|plane| plane.fd.try_clone())
                .collect::<std::io::Result<Vec<_>>>()
                .inspect_err(|error| {
                    CLONE_WARNING.call_once(|| {
                        tracing::warn!(%error, "DMA-BUF diagnostic FD duplication failed; readiness will be reported as a probe error");
                    });
                })
                .ok(),
        })
    }

    pub(crate) fn record(&self, batch_size: u64) {
        let Some(observer) = OBSERVER.get() else {
            return;
        };
        let readiness = self
            .planes
            .as_ref()
            .filter(|planes| !planes.is_empty())
            .map(|planes| {
                let mut readiness = Readiness::Ready;
                for plane in planes {
                    let mut pollfd = libc::pollfd {
                        fd: plane.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: pollfd is writable and valid for one element. A zero
                    // timeout only queries dma_resv write-fence readiness; it does
                    // not consume a fence, wait for it or change ownership.
                    let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
                    match classify_poll(result, pollfd.revents) {
                        Readiness::Error => return Readiness::Error,
                        Readiness::Unready => readiness = Readiness::Unready,
                        Readiness::Ready => {}
                    }
                }
                readiness
            })
            .unwrap_or(Readiness::Error);
        let detail = detail(self.explicit, readiness);
        let _ = std::panic::catch_unwind(|| observer(self.buffer_id.0, detail, batch_size));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Readiness {
    Ready,
    Unready,
    Error,
}

fn classify_poll(result: libc::c_int, revents: libc::c_short) -> Readiness {
    if result < 0 || revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        Readiness::Error
    } else if result > 0 && revents & libc::POLLIN != 0 {
        Readiness::Ready
    } else {
        Readiness::Unready
    }
}

fn detail(explicit: bool, readiness: Readiness) -> u64 {
    match readiness {
        Readiness::Ready => 2 * u64::from(explicit),
        Readiness::Unready => 2 * u64::from(explicit) + 1,
        Readiness::Error => 4 + u64::from(explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_errors_are_not_reported_as_ready_or_busy() {
        assert_eq!(classify_poll(0, 0), Readiness::Unready);
        assert_eq!(classify_poll(1, libc::POLLIN), Readiness::Ready);
        for error in [libc::POLLERR, libc::POLLHUP, libc::POLLNVAL] {
            assert_eq!(classify_poll(1, libc::POLLIN | error), Readiness::Error);
        }
        assert_eq!(classify_poll(-1, 0), Readiness::Error);
        for (readiness, implicit, explicit) in [
            (Readiness::Ready, 0, 2),
            (Readiness::Unready, 1, 3),
            (Readiness::Error, 4, 5),
        ] {
            assert_eq!(detail(false, readiness), implicit);
            assert_eq!(detail(true, readiness), explicit);
        }
    }
}
