//! GBM-backed buffers and the compositor-owned scanout slot ledger.
//!
//! The pool owns GBM allocations and the compositor's scanout slot ledger.
//! Atomic presentation creates DRM framebuffer objects for these allocations,
//! but only the state machine decides when their storage is reusable.

#![cfg_attr(test, allow(dead_code))]

use std::{
    fmt,
    num::NonZeroUsize,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    time::Instant,
};

use cosmix_wgpu_dmabuf::{
    DmabufDescriptor, DmabufPlane, RETIREMENT_WAIT_TIMEOUT, ScanoutRenderBridge,
    ScanoutRenderTarget, WaitForSubmittedWork,
};
use smithay::backend::allocator::{
    Buffer, Fourcc, Modifier,
    dmabuf::{AsDmabuf, Dmabuf},
    gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice},
};

use super::kms::AtomicOutputSelection;

pub(crate) const DEFAULT_SCANOUT_SLOTS: usize = 2;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const MAX_SCANOUT_SLOTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScanoutPoolConfig {
    slot_count: NonZeroUsize,
}

impl ScanoutPoolConfig {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(slot_count: usize) -> Result<Self, ScanoutPoolError> {
        let slot_count = NonZeroUsize::new(slot_count)
            .filter(|slot_count| slot_count.get() >= DEFAULT_SCANOUT_SLOTS)
            .ok_or_else(|| {
                ScanoutPoolError::new(format!(
                    "scanout pool must contain at least {DEFAULT_SCANOUT_SLOTS} slots"
                ))
            })?;
        if slot_count.get() > MAX_SCANOUT_SLOTS {
            return Err(ScanoutPoolError::new(format!(
                "scanout pool slot count {} exceeds the bounded maximum {MAX_SCANOUT_SLOTS}",
                slot_count.get()
            )));
        }
        Ok(Self { slot_count })
    }

    pub(crate) fn two_slot() -> Self {
        Self {
            slot_count: NonZeroUsize::new(DEFAULT_SCANOUT_SLOTS)
                .expect("two-slot scanout pool is non-zero"),
        }
    }

    fn len(self) -> usize {
        self.slot_count.get()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScanoutPoolError(String);

impl ScanoutPoolError {
    fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

impl fmt::Display for ScanoutPoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScanoutPoolError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScanoutSlotState {
    Free,
    Rendering,
    Queued,
    Front,
    HeldUntilSuspend,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ScanoutSlotId(pub(crate) usize);

#[derive(Clone, Debug)]
struct ScanoutSlotMachine {
    states: Vec<ScanoutSlotState>,
    next_candidate: usize,
}

impl ScanoutSlotMachine {
    fn new(slot_count: usize) -> Self {
        Self {
            states: vec![ScanoutSlotState::Free; slot_count],
            next_candidate: 0,
        }
    }

    fn state(&self, slot: ScanoutSlotId) -> Result<ScanoutSlotState, ScanoutPoolError> {
        self.states
            .get(slot.0)
            .copied()
            .ok_or_else(|| ScanoutPoolError::new(format!("unknown scanout slot {}", slot.0)))
    }

    fn transition(
        &mut self,
        slot: ScanoutSlotId,
        expected: &[ScanoutSlotState],
        next: ScanoutSlotState,
    ) -> Result<(), ScanoutPoolError> {
        let current = self.state(slot)?;
        if !expected.contains(&current) {
            return Err(ScanoutPoolError::new(format!(
                "illegal scanout slot {} transition {current:?} -> {next:?}",
                slot.0
            )));
        }
        self.states[slot.0] = next;
        Ok(())
    }

    fn begin_rendering(&mut self) -> Result<ScanoutSlotId, ScanoutPoolError> {
        for offset in 0..self.states.len() {
            let index = (self.next_candidate + offset) % self.states.len();
            if self.states[index] == ScanoutSlotState::Free {
                self.states[index] = ScanoutSlotState::Rendering;
                self.next_candidate = (index + 1) % self.states.len();
                return Ok(ScanoutSlotId(index));
            }
        }
        Err(ScanoutPoolError::new(
            "scanout pool has no reusable Free slot",
        ))
    }

    fn queue(&mut self, slot: ScanoutSlotId) -> Result<(), ScanoutPoolError> {
        self.transition(
            slot,
            &[ScanoutSlotState::Rendering],
            ScanoutSlotState::Queued,
        )
    }

    fn queue_retained_candidate(&mut self, slot: ScanoutSlotId) -> Result<(), ScanoutPoolError> {
        self.transition(slot, &[ScanoutSlotState::Free], ScanoutSlotState::Queued)
    }

    fn abandon_uncommitted_retained_candidate(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        self.transition(slot, &[ScanoutSlotState::Queued], ScanoutSlotState::Free)
    }

    fn display_queued(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<Option<ScanoutSlotId>, ScanoutPoolError> {
        if self.state(slot)? != ScanoutSlotState::Queued {
            return Err(ScanoutPoolError::new(format!(
                "illegal scanout slot {} transition {:?} -> Front",
                slot.0,
                self.state(slot)?
            )));
        }
        let old_front = self
            .states
            .iter()
            .position(|state| *state == ScanoutSlotState::Front)
            .map(ScanoutSlotId);
        if let Some(old_front) = old_front {
            self.states[old_front.0] = ScanoutSlotState::Free;
        }
        self.states[slot.0] = ScanoutSlotState::Front;
        Ok(old_front)
    }

    fn cancel(&mut self, slot: ScanoutSlotId) -> Result<(), ScanoutPoolError> {
        self.transition(
            slot,
            &[
                ScanoutSlotState::Rendering,
                ScanoutSlotState::Queued,
                ScanoutSlotState::Front,
            ],
            ScanoutSlotState::HeldUntilSuspend,
        )
    }

    fn settle_unpresented_after_retirement(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        let state = self.state(slot)?;
        if state != ScanoutSlotState::Rendering {
            return Err(ScanoutPoolError::new(format!(
                "unpresented scanout slot {} settled from {state:?}, expected Rendering",
                slot.0
            )));
        }
        self.cancel(slot)
    }

    fn release_after_suspend(&mut self) -> Vec<ScanoutSlotId> {
        let mut retired = Vec::new();
        for (index, state) in self.states.iter_mut().enumerate() {
            match state {
                ScanoutSlotState::Queued | ScanoutSlotState::Front => {
                    *state = ScanoutSlotState::Free;
                }
                ScanoutSlotState::HeldUntilSuspend => {
                    // A held allocation has either unproved GPU completion or
                    // a cancelled commit lifetime. Suspending permits its
                    // storage to be dropped, never recycled. Only complete
                    // replacement storage can make this index Free again.
                    retired.push(ScanoutSlotId(index));
                }
                ScanoutSlotState::Free | ScanoutSlotState::Rendering => {}
            }
        }
        retired
    }

    fn mark_disabled(&mut self) -> Result<(), ScanoutPoolError> {
        if let Some((index, _)) = self
            .states
            .iter()
            .enumerate()
            .find(|(_, state)| **state == ScanoutSlotState::Rendering)
        {
            return Err(ScanoutPoolError::new(format!(
                "cannot mark scanout disabled while slot {index} is Rendering"
            )));
        }
        for state in &mut self.states {
            if matches!(state, ScanoutSlotState::Queued | ScanoutSlotState::Front) {
                *state = ScanoutSlotState::Free;
            }
        }
        Ok(())
    }

    fn state_view(&self) -> Vec<(ScanoutSlotId, ScanoutSlotState)> {
        self.states
            .iter()
            .copied()
            .enumerate()
            .map(|(index, state)| (ScanoutSlotId(index), state))
            .collect()
    }
}

struct ScanoutAllocation {
    buffer: GbmBuffer,
    target: ScanoutRenderTarget,
}

/// Plane-fd ownership for the last displayed allocation across a paused
/// dwell. It contains no framebuffer ID, GBM device or wgpu/Vulkan target;
/// those are always recreated from the resumed authority generation.
#[derive(Clone, Debug)]
pub(crate) struct RetainedScanoutBuffer {
    selection: AtomicOutputSelection,
    dmabuf: Dmabuf,
}

impl RetainedScanoutBuffer {
    pub(crate) fn selection(&self) -> AtomicOutputSelection {
        self.selection
    }
}

struct ScanoutSlotStorage<T> {
    machine: ScanoutSlotMachine,
    slots: Vec<Option<T>>,
}

impl<T> ScanoutSlotStorage<T> {
    fn new(slots: Vec<T>) -> Result<Self, ScanoutPoolError> {
        if slots.is_empty() {
            return Err(ScanoutPoolError::new(
                "scanout slot storage must not be empty",
            ));
        }
        Ok(Self {
            machine: ScanoutSlotMachine::new(slots.len()),
            slots: slots.into_iter().map(Some).collect(),
        })
    }

    fn release_after_suspend(&mut self) {
        for slot in self.machine.release_after_suspend() {
            drop(self.slots[slot.0].take());
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn replace(&mut self, slots: Vec<T>) -> Result<(), ScanoutPoolError> {
        let replacement = Self::new(slots)?;
        *self = replacement;
        Ok(())
    }
}

pub(crate) struct ScanoutPool {
    _selection: AtomicOutputSelection,
    _allocator: GbmAllocator<OwnedFd>,
    slots: ScanoutSlotStorage<ScanoutAllocation>,
    completion: cosmix_wgpu_dmabuf::WgpuWaitForSubmittedWork,
}

impl ScanoutPool {
    pub(crate) fn allocate(
        drm_fd: BorrowedFd<'_>,
        drm_device: u64,
        selection: AtomicOutputSelection,
        config: ScanoutPoolConfig,
        bridge: &ScanoutRenderBridge,
    ) -> Result<Self, ScanoutPoolError> {
        Self::allocate_inner(drm_fd, drm_device, selection, config, bridge, None, None)
            .map(|(pool, _)| pool)
    }

    /// Allocate a fresh full-modeset pool during staged resume. Unlike ordinary
    /// first-light allocation, each non-cancellable GBM/Vulkan step gates entry
    /// against the session's existing resume deadline.
    pub(crate) fn allocate_with_staged_deadline(
        drm_fd: BorrowedFd<'_>,
        drm_device: u64,
        selection: AtomicOutputSelection,
        config: ScanoutPoolConfig,
        bridge: &ScanoutRenderBridge,
        staged_deadline: Instant,
    ) -> Result<Self, ScanoutPoolError> {
        Self::allocate_inner(
            drm_fd,
            drm_device,
            selection,
            config,
            bridge,
            None,
            Some(staged_deadline),
        )
        .map(|(pool, _)| pool)
    }

    /// Import retained plane fds through a GBM device built from the new
    /// master lease. Slot zero is the retained candidate; every framebuffer
    /// registration and Vulkan target in the returned pool is fresh.
    pub(crate) fn allocate_with_retained(
        drm_fd: BorrowedFd<'_>,
        drm_device: u64,
        selection: AtomicOutputSelection,
        config: ScanoutPoolConfig,
        bridge: &ScanoutRenderBridge,
        retained: &RetainedScanoutBuffer,
        staged_deadline: Instant,
    ) -> Result<(Self, ScanoutSlotId), ScanoutPoolError> {
        if retained.selection != selection {
            return Err(ScanoutPoolError::new(format!(
                "retained scanout selection {:?} does not match resumed selection {:?}",
                retained.selection, selection
            )));
        }
        let (pool, slot) = Self::allocate_inner(
            drm_fd,
            drm_device,
            selection,
            config,
            bridge,
            Some(retained),
            Some(staged_deadline),
        )?;
        Ok((
            pool,
            slot.expect("retained allocation always occupies one pool slot"),
        ))
    }

    fn allocate_inner(
        drm_fd: BorrowedFd<'_>,
        drm_device: u64,
        selection: AtomicOutputSelection,
        config: ScanoutPoolConfig,
        bridge: &ScanoutRenderBridge,
        retained: Option<&RetainedScanoutBuffer>,
        staged_deadline: Option<Instant>,
    ) -> Result<(Self, Option<ScanoutSlotId>), ScanoutPoolError> {
        ensure_staged_allocation_entry(staged_deadline, "GBM device creation")?;
        let gbm_fd = drm_fd.try_clone_to_owned().map_err(|error| {
            ScanoutPoolError::new(format!("scanout GBM fd duplication failed: {error}"))
        })?;
        let gbm = GbmDevice::new(gbm_fd).map_err(|error| {
            ScanoutPoolError::new(format!("scanout GBM device creation failed: {error}"))
        })?;
        let mut allocator =
            GbmAllocator::new(gbm, GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING);
        let fourcc = Fourcc::try_from(selection.format).map_err(|_| {
            ScanoutPoolError::new(format!(
                "scanout GBM does not recognise fourcc {:#010x}",
                selection.format
            ))
        })?;
        let modifier = Modifier::from(selection.modifier);
        let mut allocations = Vec::with_capacity(config.len());
        let retained_slot = if let Some(retained) = retained {
            // GBM and Vulkan expose no cancellation/deadline parameter for
            // these calls. The staged deadline therefore gates entry to every
            // potentially blocking import/allocation step; a call already in
            // progress may still return after the deadline.
            ensure_staged_allocation_entry(staged_deadline, "retained GBM import")?;
            let buffer = retained
                .dmabuf
                .import_to(
                    allocator.as_ref(),
                    GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
                )
                .map_err(|error| {
                    ScanoutPoolError::new(format!(
                        "retained scanout GBM import on resumed lease failed: {error}"
                    ))
                })?;
            validate_gbm_buffer(&buffer, fourcc, modifier, "retained scanout")?;
            ensure_staged_allocation_entry(staged_deadline, "retained Vulkan import")?;
            allocations.push(import_scanout_allocation(
                buffer,
                drm_device,
                selection,
                bridge,
                "retained scanout",
            )?);
            Some(ScanoutSlotId(0))
        } else {
            None
        };
        for index in allocations.len()..config.len() {
            ensure_staged_allocation_entry(staged_deadline, "fresh GBM allocation")?;
            let buffer = allocator
                .create_buffer_with_flags(
                    selection.mode.width,
                    selection.mode.height,
                    fourcc,
                    &[modifier],
                    GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
                )
                .map_err(|error| {
                    ScanoutPoolError::new(format!(
                        "scanout GBM slot {index} allocation failed: {error}"
                    ))
                })?;
            validate_gbm_buffer(
                &buffer,
                fourcc,
                modifier,
                &format!("scanout GBM slot {index}"),
            )?;
            ensure_staged_allocation_entry(staged_deadline, "fresh Vulkan import")?;
            allocations.push(import_scanout_allocation(
                buffer,
                drm_device,
                selection,
                bridge,
                &format!("scanout GBM slot {index}"),
            )?);
        }
        Ok((
            Self {
                _selection: selection,
                _allocator: allocator,
                slots: ScanoutSlotStorage::new(allocations)?,
                completion: bridge.retirement_adapter(),
            },
            retained_slot,
        ))
    }

    pub(crate) fn begin_rendering(&mut self) -> Result<ScanoutSlotId, ScanoutPoolError> {
        self.slots.machine.begin_rendering()
    }

    pub(crate) fn duplicate_drm_fd(&self) -> Result<OwnedFd, ScanoutPoolError> {
        // Commit/property helpers may use this duplicate. They must never read
        // its shared DRM event queue; LiveAtomicOwnership supplies exactly one
        // per-device ProductionAtomicEventRouter for that purpose.
        self._allocator
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| {
                ScanoutPoolError::new(format!("scanout pool DRM fd duplication failed: {error}"))
            })
    }

    pub(crate) fn manual_view(
        &self,
        slot: ScanoutSlotId,
    ) -> Result<bevy::render::texture::ManualTextureView, ScanoutPoolError> {
        self.slots
            .slots
            .get(slot.0)
            .and_then(Option::as_ref)
            .map(|allocation| allocation.target.manual_view())
            .ok_or_else(|| {
                ScanoutPoolError::new(format!("scanout slot {} has no live allocation", slot.0))
            })
    }

    pub(crate) fn selection(&self) -> AtomicOutputSelection {
        self._selection
    }

    pub(crate) fn retain_front_buffer(
        &self,
    ) -> Result<Option<RetainedScanoutBuffer>, ScanoutPoolError> {
        let Some((slot, _)) = self
            .slots
            .machine
            .state_view()
            .into_iter()
            .find(|(_, state)| *state == ScanoutSlotState::Front)
        else {
            return Ok(None);
        };
        let allocation = self
            .slots
            .slots
            .get(slot.0)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                ScanoutPoolError::new(format!(
                    "front scanout slot {} has no live allocation",
                    slot.0
                ))
            })?;
        let dmabuf = allocation.buffer.export().map_err(|error| {
            ScanoutPoolError::new(format!("front scanout DMA-BUF export failed: {error}"))
        })?;
        Ok(Some(RetainedScanoutBuffer {
            selection: self._selection,
            dmabuf,
        }))
    }

    pub(crate) fn queue_retained_candidate(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        self.slots.machine.queue_retained_candidate(slot)
    }

    pub(crate) fn abandon_uncommitted_retained_candidate(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        self.slots
            .machine
            .abandon_uncommitted_retained_candidate(slot)
    }

    pub(crate) fn slot_ids(&self) -> impl Iterator<Item = ScanoutSlotId> + '_ {
        self.slots
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|_| ScanoutSlotId(index)))
    }

    pub(crate) fn gbm_buffer(&self, slot: ScanoutSlotId) -> Result<&GbmBuffer, ScanoutPoolError> {
        self.slots
            .slots
            .get(slot.0)
            .and_then(Option::as_ref)
            .map(|allocation| &allocation.buffer)
            .ok_or_else(|| {
                ScanoutPoolError::new(format!(
                    "scanout slot {} has no live GBM allocation",
                    slot.0
                ))
            })
    }

    /// Prove that rendering completed without making the allocation reusable.
    /// The caller must immediately move the still-Rendering slot to Queued or
    /// HeldUntilSuspend.
    pub(crate) fn prove_rendering_complete(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        prove_rendering_complete(&mut self.slots.machine, slot, &mut self.completion)
    }

    #[allow(dead_code)]
    pub(crate) fn queue(&mut self, slot: ScanoutSlotId) -> Result<(), ScanoutPoolError> {
        self.slots.machine.queue(slot)
    }

    #[allow(dead_code)]
    pub(crate) fn display_queued(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<Option<ScanoutSlotId>, ScanoutPoolError> {
        self.slots.machine.display_queued(slot)
    }

    #[allow(dead_code)]
    pub(crate) fn cancel(&mut self, slot: ScanoutSlotId) -> Result<(), ScanoutPoolError> {
        self.slots.machine.cancel(slot)
    }

    /// Global GPU retirement proved that an acquired frame which never reached
    /// presentation is no longer being written. It remains non-reusable until
    /// suspend destroys or replaces its storage.
    pub(crate) fn settle_unpresented_after_retirement(
        &mut self,
        slot: ScanoutSlotId,
    ) -> Result<(), ScanoutPoolError> {
        self.slots.machine.settle_unpresented_after_retirement(slot)
    }

    /// Record a proved disable before framebuffer removal. This changes only
    /// the userspace slot ledger; storage stays owned until RmFB succeeds.
    pub(crate) fn mark_disabled(&mut self) -> Result<(), ScanoutPoolError> {
        self.slots.machine.mark_disabled()
    }

    pub(crate) fn slot_state_view(&self) -> Vec<(ScanoutSlotId, ScanoutSlotState)> {
        self.slots.machine.state_view()
    }

    #[allow(dead_code)]
    pub(crate) fn release_after_suspend(&mut self) {
        self.slots.release_after_suspend();
    }
}

fn validate_gbm_buffer(
    buffer: &GbmBuffer,
    expected_fourcc: Fourcc,
    expected_modifier: Modifier,
    label: &str,
) -> Result<(), ScanoutPoolError> {
    let actual = Buffer::format(buffer);
    if actual.code == expected_fourcc && actual.modifier == expected_modifier {
        Ok(())
    } else {
        Err(ScanoutPoolError::new(format!(
            "{label} returned {:?}/{:#018x}, expected {:?}/{:#018x}",
            actual.code,
            u64::from(actual.modifier),
            expected_fourcc,
            u64::from(expected_modifier)
        )))
    }
}

fn ensure_staged_allocation_entry(
    staged_deadline: Option<Instant>,
    phase: &'static str,
) -> Result<(), ScanoutPoolError> {
    if staged_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(ScanoutPoolError::new(format!(
            "staged resume deadline expired before {phase}"
        )));
    }
    Ok(())
}

fn import_scanout_allocation(
    buffer: GbmBuffer,
    drm_device: u64,
    selection: AtomicOutputSelection,
    bridge: &ScanoutRenderBridge,
    label: &str,
) -> Result<ScanoutAllocation, ScanoutPoolError> {
    let dmabuf = buffer
        .export()
        .map_err(|error| ScanoutPoolError::new(format!("{label} export failed: {error}")))?;
    let planes = dmabuf
        .handles()
        .zip(dmabuf.offsets())
        .zip(dmabuf.strides())
        .map(|((fd, offset), stride)| {
            fd.try_clone_to_owned()
                .map(|fd| DmabufPlane { fd, offset, stride })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            ScanoutPoolError::new(format!("{label} plane duplication failed: {error}"))
        })?;
    let target = bridge
        .import(
            drm_device,
            DmabufDescriptor {
                width: selection.mode.width,
                height: selection.mode.height,
                fourcc: selection.format,
                modifier: selection.modifier,
                planes,
            },
        )
        .map_err(|error| ScanoutPoolError::new(format!("{label} Vulkan import failed: {error}")))?;
    Ok(ScanoutAllocation { buffer, target })
}

fn prove_rendering_complete(
    machine: &mut ScanoutSlotMachine,
    slot: ScanoutSlotId,
    completion: &mut impl WaitForSubmittedWork,
) -> Result<(), ScanoutPoolError> {
    if machine.state(slot)? != ScanoutSlotState::Rendering {
        return Err(ScanoutPoolError::new(format!(
            "scanout GPU completion requested for non-Rendering slot {}",
            slot.0
        )));
    }
    match completion.wait_for_submitted_work(RETIREMENT_WAIT_TIMEOUT) {
        Ok(()) => Ok(()),
        Err(error) => {
            machine.cancel(slot)?;
            Err(ScanoutPoolError::new(format!(
                "scanout GPU completion remained unproven within {}ms: {error}",
                RETIREMENT_WAIT_TIMEOUT.as_millis()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use super::*;
    use cosmix_wgpu_dmabuf::RetirementWaitError;

    struct ScriptedCompletion(Result<(), RetirementWaitError>);

    impl WaitForSubmittedWork for ScriptedCompletion {
        fn wait_for_submitted_work(
            &mut self,
            timeout: Duration,
        ) -> Result<(), RetirementWaitError> {
            assert_eq!(timeout, RETIREMENT_WAIT_TIMEOUT);
            self.0.clone()
        }
    }

    fn machine_in(state: ScanoutSlotState) -> ScanoutSlotMachine {
        ScanoutSlotMachine {
            states: vec![state],
            next_candidate: 0,
        }
    }

    #[test]
    fn slot_state_machine_accepts_only_the_declared_transitions() {
        for initial in [
            ScanoutSlotState::Free,
            ScanoutSlotState::Rendering,
            ScanoutSlotState::Queued,
            ScanoutSlotState::Front,
            ScanoutSlotState::HeldUntilSuspend,
        ] {
            let mut begin = machine_in(initial);
            assert_eq!(
                begin.begin_rendering().is_ok(),
                initial == ScanoutSlotState::Free,
                "begin from {initial:?}"
            );

            let mut queue = machine_in(initial);
            assert_eq!(
                queue.queue(ScanoutSlotId(0)).is_ok(),
                initial == ScanoutSlotState::Rendering,
                "queue from {initial:?}"
            );

            let mut retained_queue = machine_in(initial);
            assert_eq!(
                retained_queue
                    .queue_retained_candidate(ScanoutSlotId(0))
                    .is_ok(),
                initial == ScanoutSlotState::Free,
                "retained queue from {initial:?}"
            );

            let mut retained_abandon = machine_in(initial);
            assert_eq!(
                retained_abandon
                    .abandon_uncommitted_retained_candidate(ScanoutSlotId(0))
                    .is_ok(),
                initial == ScanoutSlotState::Queued,
                "retained abandon from {initial:?}"
            );

            let mut front = machine_in(initial);
            assert_eq!(
                front.display_queued(ScanoutSlotId(0)).is_ok(),
                initial == ScanoutSlotState::Queued,
                "front from {initial:?}"
            );

            let mut cancel = machine_in(initial);
            assert_eq!(
                cancel.cancel(ScanoutSlotId(0)).is_ok(),
                matches!(
                    initial,
                    ScanoutSlotState::Rendering
                        | ScanoutSlotState::Queued
                        | ScanoutSlotState::Front
                ),
                "cancel from {initial:?}"
            );
        }
    }

    #[test]
    fn queued_front_and_cancelled_slots_are_not_reused_before_suspend() {
        let mut machine = ScanoutSlotMachine::new(3);
        let queued = machine.begin_rendering().expect("queued slot");
        machine.queue(queued).expect("slot queues");
        let front = machine.begin_rendering().expect("front slot");
        machine.queue(front).expect("front candidate queues");
        machine.display_queued(front).expect("candidate fronts");
        let cancelled = machine.begin_rendering().expect("cancelled slot");
        machine.cancel(cancelled).expect("slot cancels");
        assert!(machine.begin_rendering().is_err());

        assert_eq!(machine.release_after_suspend(), vec![cancelled]);
        assert!(machine.begin_rendering().is_ok());
    }

    #[test]
    fn old_front_becomes_free_only_after_the_queued_flip_completes() {
        let mut machine = ScanoutSlotMachine::new(2);
        let first = machine.begin_rendering().expect("first slot");
        machine.queue(first).expect("first queues");
        assert_eq!(machine.display_queued(first).expect("first fronts"), None);
        let second = machine.begin_rendering().expect("second slot");
        machine.queue(second).expect("second queues");
        assert_eq!(machine.state(first).unwrap(), ScanoutSlotState::Front);
        assert_eq!(
            machine.display_queued(second).expect("second fronts"),
            Some(first)
        );
        assert_eq!(machine.state(first).unwrap(), ScanoutSlotState::Free);
        assert_eq!(machine.state(second).unwrap(), ScanoutSlotState::Front);
    }

    #[test]
    fn gpu_timeout_holds_unpresented_rendering_slot_until_suspend() {
        let mut machine = ScanoutSlotMachine::new(1);
        let slot = machine.begin_rendering().expect("rendering slot");
        let error = prove_rendering_complete(
            &mut machine,
            slot,
            &mut ScriptedCompletion(Err(RetirementWaitError::Timeout)),
        )
        .expect_err("timeout is terminal to this use");
        assert!(error.to_string().contains("remained unproven"));
        assert_eq!(
            machine.state(slot).unwrap(),
            ScanoutSlotState::HeldUntilSuspend
        );
        assert!(machine.begin_rendering().is_err());
        assert_eq!(machine.release_after_suspend(), vec![slot]);
        assert_eq!(
            machine.state(slot).unwrap(),
            ScanoutSlotState::HeldUntilSuspend
        );
        assert!(machine.begin_rendering().is_err());
    }

    #[test]
    fn cancelled_committed_slot_is_held_until_suspend() {
        let mut machine = ScanoutSlotMachine::new(2);
        let slot = machine.begin_rendering().expect("rendering slot");
        machine.queue(slot).expect("committed slot queues");
        machine.cancel(slot).expect("cancelled commit is held");
        assert_eq!(
            machine.state(slot).unwrap(),
            ScanoutSlotState::HeldUntilSuspend
        );
        assert_eq!(machine.release_after_suspend(), vec![slot]);
        assert_eq!(
            machine.state(slot).unwrap(),
            ScanoutSlotState::HeldUntilSuspend
        );
    }

    #[test]
    fn proved_disable_clears_front_and_queued_ledger_states_without_dropping_storage() {
        let mut machine = ScanoutSlotMachine::new(2);
        let front = machine.begin_rendering().expect("front rendering slot");
        machine.queue(front).expect("front slot queues");
        machine.display_queued(front).expect("front slot displays");
        let queued = machine.begin_rendering().expect("queued rendering slot");
        machine.queue(queued).expect("second slot queues");

        machine.mark_disabled().expect("disable was proved");

        assert_eq!(
            machine.state_view(),
            [
                (ScanoutSlotId(0), ScanoutSlotState::Free),
                (ScanoutSlotId(1), ScanoutSlotState::Free),
            ]
        );
    }

    #[test]
    fn disable_ledger_transition_refuses_a_rendering_slot() {
        let mut machine = ScanoutSlotMachine::new(2);
        let slot = machine.begin_rendering().expect("rendering slot");
        let error = machine
            .mark_disabled()
            .expect_err("rendering storage is not quiesced");
        assert!(error.to_string().contains("is Rendering"));
        assert_eq!(machine.state(slot).unwrap(), ScanoutSlotState::Rendering);
    }

    #[test]
    fn pause_immediately_after_install_tears_down_cleanly_and_reaches_disable() {
        let mut machine = ScanoutSlotMachine::new(2);
        let first = machine
            .begin_rendering()
            .expect("source installation pre-acquires its first slot");

        machine
            .settle_unpresented_after_retirement(first)
            .expect("proved global retirement settles the unpresented slot");
        machine
            .mark_disabled()
            .expect("disable sees no live Rendering slot");

        assert_eq!(
            machine.state(first).unwrap(),
            ScanoutSlotState::HeldUntilSuspend
        );
    }

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn held_storage_is_retired_at_suspend_and_only_replacement_reenters_service() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut storage =
            ScanoutSlotStorage::new(vec![DropCount(Arc::clone(&drops))]).expect("storage");
        let slot = storage.machine.begin_rendering().expect("rendering slot");
        storage.machine.cancel(slot).expect("slot is held");

        storage.release_after_suspend();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(storage.slots[slot.0].is_none());
        assert!(storage.machine.begin_rendering().is_err());

        storage
            .replace(vec![DropCount(Arc::clone(&drops))])
            .expect("fresh allocation replaces retired storage");
        assert_eq!(storage.machine.begin_rendering().unwrap(), slot);
        drop(storage);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn target_replacement_drops_every_old_slot_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut storage = ScanoutSlotStorage::new(vec![
            DropCount(Arc::clone(&drops)),
            DropCount(Arc::clone(&drops)),
        ])
        .expect("old storage");
        storage
            .replace(vec![
                DropCount(Arc::clone(&drops)),
                DropCount(Arc::clone(&drops)),
            ])
            .expect("replacement storage");
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        drop(storage);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn slot_count_is_bounded_and_two_is_the_production_default() {
        assert_eq!(ScanoutPoolConfig::two_slot().len(), 2);
        assert!(ScanoutPoolConfig::new(0).is_err());
        assert!(ScanoutPoolConfig::new(1).is_err());
        assert!(ScanoutPoolConfig::new(MAX_SCANOUT_SLOTS).is_ok());
        assert!(ScanoutPoolConfig::new(MAX_SCANOUT_SLOTS + 1).is_err());
    }

    #[test]
    fn staged_deadline_gates_retained_and_fallback_allocation_steps_before_entry() {
        let now = Instant::now();
        for phase in ["retained Vulkan import", "fallback fresh GBM allocation"] {
            let error = ensure_staged_allocation_entry(
                Some(now - std::time::Duration::from_millis(1)),
                phase,
            )
            .expect_err("expired stage refuses a new non-cancellable operation");
            assert!(error.to_string().contains(phase));
            ensure_staged_allocation_entry(Some(now + std::time::Duration::from_secs(1)), phase)
                .expect("live stage admits the next operation");
        }
    }
}
