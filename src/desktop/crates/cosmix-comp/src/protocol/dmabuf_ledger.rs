//! Observed linux-dmabuf import outcomes (TODO-comp C4).
//!
//! What a driver *claims* to support (the advertised format/modifier table)
//! and what it actually *accepted* are different facts; only the second is
//! true. Every `zwp_linux_buffer_params_v1` import that comp answers is
//! counted here, and every refusal keeps a record of its format, modifier
//! and reason, so a "works on AMD, black screen on the VM" bug is readable
//! from `comp.props.get dmabuf` instead of reproducible only.
//!
//! Shared by the protocol thread (metadata, descriptor and queue refusals)
//! and the Vulkan validation worker (the real test import). In memory only:
//! a comp restart starts from zero, and the props description says so.
//! Demoting the advertised set from these observations is a later step.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use smithay::backend::allocator::Format;

/// How many refusals the ledger keeps, newest last.
pub(crate) const DMABUF_FAILURE_RING: usize = 16;

/// Why an import was refused. The wire spelling is [`Self::as_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DmabufFailureReason {
    /// Size, plane count or format failed comp's own metadata checks.
    InvalidMetadata,
    /// The plane descriptor could not be duplicated for the worker.
    DescriptorDupFailed,
    /// The validation queue was full; refused without blocking protocol.
    QueueFull,
    /// The validation worker is gone.
    WorkerStopped,
    /// The Vulkan test import rejected the buffer: the driver said no.
    VulkanRejected,
    /// The validation probe panicked on this buffer and was retired.
    ProbePanicked,
    /// Refused because an earlier panic retired the probe.
    ProbeRetired,
}

impl DmabufFailureReason {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 7] = [
        Self::InvalidMetadata,
        Self::DescriptorDupFailed,
        Self::QueueFull,
        Self::WorkerStopped,
        Self::VulkanRejected,
        Self::ProbePanicked,
        Self::ProbeRetired,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMetadata => "invalid_metadata",
            Self::DescriptorDupFailed => "descriptor_dup_failed",
            Self::QueueFull => "queue_full",
            Self::WorkerStopped => "worker_stopped",
            Self::VulkanRejected => "vulkan_rejected",
            Self::ProbePanicked => "probe_panicked",
            Self::ProbeRetired => "probe_retired",
        }
    }
}

/// One refused import, as `dmabuf.failures[]` serves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct DmabufFailureRecord {
    /// DRM fourcc as its four characters (`"AR24"`); a non-printable byte
    /// from a hostile or corrupt code reads `?`.
    pub(crate) format: String,
    /// DRM format modifier, `0x` + 16 hex digits.
    pub(crate) modifier: String,
    /// [`DmabufFailureReason::as_str`].
    pub(crate) reason: &'static str,
    /// The refusing check's own message.
    pub(crate) detail: String,
    /// CLOCK_MONOTONIC µs when comp refused it.
    pub(crate) at_us: u64,
}

/// A consistent copy of the ledger for one read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct DmabufLedgerSnapshot {
    pub(crate) accepted: u64,
    pub(crate) failed: u64,
    pub(crate) failures: Vec<DmabufFailureRecord>,
}

#[derive(Debug, Default)]
struct Inner {
    accepted: u64,
    failed: u64,
    failures: VecDeque<DmabufFailureRecord>,
}

/// Cheap to clone: every clone is the same ledger.
#[derive(Clone, Debug, Default)]
pub(crate) struct DmabufImportLedger(Arc<Mutex<Inner>>);

impl DmabufImportLedger {
    pub(crate) fn record_accepted(&self) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.accepted = inner.accepted.saturating_add(1);
    }

    pub(crate) fn record_failed(
        &self,
        format: Format,
        reason: DmabufFailureReason,
        detail: impl Into<String>,
    ) {
        let record = DmabufFailureRecord {
            format: fourcc_text(format.code as u32),
            modifier: format!("{:#018x}", u64::from(format.modifier)),
            reason: reason.as_str(),
            detail: detail.into(),
            at_us: super::monotonic_micros(),
        };
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.failed = inner.failed.saturating_add(1);
        if inner.failures.len() == DMABUF_FAILURE_RING {
            inner.failures.pop_front();
        }
        inner.failures.push_back(record);
    }

    #[cfg_attr(not(any(feature = "bus", test)), allow(dead_code))]
    pub(crate) fn snapshot(&self) -> DmabufLedgerSnapshot {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        DmabufLedgerSnapshot {
            accepted: inner.accepted,
            failed: inner.failed,
            failures: inner.failures.iter().cloned().collect(),
        }
    }
}

fn fourcc_text(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() || byte == b' ' {
                char::from(byte)
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::allocator::{Fourcc, Modifier};

    fn argb_linear() -> Format {
        Format {
            code: Fourcc::Argb8888,
            modifier: Modifier::Linear,
        }
    }

    #[test]
    fn a_refusal_records_format_modifier_and_reason() {
        let ledger = DmabufImportLedger::default();
        ledger.record_accepted();
        ledger.record_failed(
            Format {
                code: Fourcc::Xrgb8888,
                modifier: Modifier::from(0x0100_0000_0000_0001_u64),
            },
            DmabufFailureReason::VulkanRejected,
            "VK_ERROR_INVALID_EXTERNAL_HANDLE",
        );
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.accepted, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(
            snapshot.failures,
            vec![DmabufFailureRecord {
                format: "XR24".into(),
                modifier: "0x0100000000000001".into(),
                reason: "vulkan_rejected",
                detail: "VK_ERROR_INVALID_EXTERNAL_HANDLE".into(),
                at_us: snapshot.failures[0].at_us,
            }]
        );
        assert!(snapshot.failures[0].at_us > 0);
    }

    #[test]
    fn the_ring_keeps_the_newest_refusals_and_the_count_keeps_them_all() {
        let ledger = DmabufImportLedger::default();
        for n in 0..(DMABUF_FAILURE_RING + 3) {
            ledger.record_failed(argb_linear(), DmabufFailureReason::QueueFull, n.to_string());
        }
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.failed, (DMABUF_FAILURE_RING + 3) as u64);
        assert_eq!(snapshot.failures.len(), DMABUF_FAILURE_RING);
        assert_eq!(snapshot.failures[0].detail, "3");
        assert_eq!(
            snapshot.failures.last().unwrap().detail,
            (DMABUF_FAILURE_RING + 2).to_string()
        );
        assert_eq!(snapshot.failures[0].modifier, "0x0000000000000000");
    }

    #[test]
    fn reason_spellings_are_distinct_snake_case() {
        let names = DmabufFailureReason::ALL.map(DmabufFailureReason::as_str);
        let unique = names.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), names.len());
        assert!(
            names
                .iter()
                .all(|name| name.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        );
    }

    #[test]
    fn a_non_printable_fourcc_cannot_reach_the_props_tree_raw() {
        assert_eq!(fourcc_text(u32::from_le_bytes(*b"AR24")), "AR24");
        assert_eq!(fourcc_text(u32::from_le_bytes([b'A', 0, 0xff, b'4'])), "A??4");
    }
}
