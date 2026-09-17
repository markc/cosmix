//! `wl_shm` buffers: a small per-surface swapchain of release-aware slots.
//!
//! The newest committed buffer is reused whenever the compositor has
//! released it, so a steady-state frame copies nothing. When it is still held,
//! another free slot is taken and brought up to date by copying only the
//! damage committed since that slot's contents were current.

use crate::geom::{Damage, Rect};
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use std::collections::VecDeque;

/// Slots per surface. Two is enough for a compositor that releases on
/// commit; the third covers one that holds a buffer across a frame.
pub const MAX_SLOTS: usize = 3;
const HISTORY: usize = 8;

/// What the pure chooser sees of a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotView {
    pub busy: bool,
    /// Sequence number of the frame whose contents the slot holds.
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotChoice {
    /// Draw into this slot; it already holds the newest contents.
    Current(usize),
    /// Draw into this slot after copying forward from the newest slot.
    Stale(usize),
    /// Allocate a new slot.
    Allocate,
    /// Every slot is held by the compositor; wait for a release.
    Wait,
}

pub fn choose_slot(slots: &[SlotView], newest: Option<u64>, max: usize) -> SlotChoice {
    if let Some(i) = slots
        .iter()
        .position(|s| !s.busy && s.seq.is_some() && s.seq == newest)
    {
        return SlotChoice::Current(i);
    }
    // Prefer the most recent free slot: it needs the least copying.
    if let Some((i, _)) = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.busy)
        .max_by_key(|(_, s)| s.seq.map_or(0, |v| v + 1))
    {
        return SlotChoice::Stale(i);
    }
    if slots.len() < max {
        SlotChoice::Allocate
    } else {
        SlotChoice::Wait
    }
}

/// What a draw that committed nothing leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discard {
    /// Nothing was written: the slot still holds what it held.
    Keep,
    /// This slot's contents are unknown now.
    Forget(usize),
    /// The newest contents are gone: everything must be redrawn.
    ForgetAll,
}

pub fn discard_choice(newest: Option<usize>, index: usize, touched: bool) -> Discard {
    if !touched {
        Discard::Keep
    } else if newest != Some(index) {
        Discard::Forget(index)
    } else {
        Discard::ForgetAll
    }
}

/// Damage of recent commits, for bringing an older slot up to date.
#[derive(Debug, Default)]
pub struct DamageHistory {
    frames: VecDeque<(u64, Vec<Rect>)>,
}

impl DamageHistory {
    pub fn push(&mut self, seq: u64, rects: &[Rect]) {
        if self.frames.len() == HISTORY {
            self.frames.pop_front();
        }
        self.frames.push_back((seq, rects.to_vec()));
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Everything that changed after `since`, up to the newest frame.
    /// `None` means the history does not reach back far enough: copy all.
    pub fn since(&self, since: Option<u64>, width: u32, height: u32) -> Option<Vec<Rect>> {
        let since = since?;
        let first = self.frames.front()?.0;
        if since + 1 < first {
            return None;
        }
        let mut damage = Damage::new(width, height);
        for (seq, rects) in &self.frames {
            if *seq > since {
                damage.extend(rects);
            }
        }
        Some(damage.rects().to_vec())
    }
}

struct Slot {
    pool: SlotPool,
    buffer: Buffer,
    seq: Option<u64>,
}

pub(crate) struct Swapchain {
    slots: Vec<Slot>,
    size: (u32, u32),
    seq: u64,
    newest: Option<usize>,
    history: DamageHistory,
    pub(crate) allocations: u64,
}

pub(crate) enum AcquireError {
    /// Every slot is held; the compositor's release wakes the loop.
    Held,
    /// A new buffer could not be made; nothing will wake the loop for it.
    Alloc(String),
}

pub(crate) struct Acquired {
    pub index: usize,
    /// True when the buffer's contents are undefined (new or resized).
    pub fresh: bool,
}

impl Swapchain {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            size: (0, 0),
            seq: 0,
            newest: None,
            history: DamageHistory::default(),
            allocations: 0,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Get a buffer to draw the next frame into.
    pub fn acquire(
        &mut self,
        shm: &Shm,
        width: u32,
        height: u32,
    ) -> Result<Acquired, AcquireError> {
        if self.size != (width, height) {
            // Dropping a held buffer defers its destruction to the release.
            self.slots.clear();
            self.newest = None;
            self.history.clear();
            self.size = (width, height);
        }
        let views: Vec<SlotView> = self
            .slots
            .iter_mut()
            .map(|s| SlotView {
                busy: s.buffer.canvas(&mut s.pool).is_none(),
                seq: s.seq,
            })
            .collect();
        let newest_seq = self.newest.and_then(|i| self.slots[i].seq);
        match choose_slot(&views, newest_seq, MAX_SLOTS) {
            SlotChoice::Current(index) => Ok(Acquired {
                index,
                fresh: false,
            }),
            SlotChoice::Stale(index) => {
                let fresh = !self.copy_forward(index);
                Ok(Acquired { index, fresh })
            }
            SlotChoice::Allocate => {
                let too_big = || AcquireError::Alloc(format!("{width}x{height} is too large"));
                let stride = width
                    .checked_mul(4)
                    .filter(|s| i32::try_from(*s).is_ok())
                    .ok_or_else(too_big)?;
                let len = (stride as usize)
                    .checked_mul(height as usize)
                    .ok_or_else(too_big)?;
                let mut pool = SlotPool::new(len.max(4), shm)
                    .map_err(|e| AcquireError::Alloc(format!("wl_shm pool: {e}")))?;
                let (buffer, _) = pool
                    .create_buffer(
                        width as i32,
                        height as i32,
                        stride as i32,
                        wl_shm::Format::Argb8888,
                    )
                    .map_err(|e| AcquireError::Alloc(format!("wl_shm buffer: {e}")))?;
                self.allocations += 1;
                self.slots.push(Slot {
                    pool,
                    buffer,
                    seq: None,
                });
                let index = self.slots.len() - 1;
                let fresh = !self.copy_forward(index);
                Ok(Acquired { index, fresh })
            }
            SlotChoice::Wait => Err(AcquireError::Held),
        }
    }

    /// Copy the damage since `index`'s contents into it from the newest
    /// slot. Returns false when there is nothing to copy from.
    fn copy_forward(&mut self, index: usize) -> bool {
        let Some(newest) = self.newest else {
            return false;
        };
        if newest == index {
            return true;
        }
        let (w, h) = self.size;
        let rects = self
            .history
            .since(self.slots[index].seq, w, h)
            .unwrap_or_else(|| vec![Rect::new(0, 0, w as i32, h as i32)]);
        let (dst, src) = pair_mut(&mut self.slots, index, newest);
        let src_slot = src.buffer.slot();
        let src_bytes = src.pool.raw_data_mut(&src_slot);
        let Some(dst_bytes) = dst.buffer.canvas(&mut dst.pool) else {
            return false;
        };
        let stride = w as usize * 4;
        for r in rects {
            let x0 = r.x as usize * 4;
            let x1 = r.right() as usize * 4;
            for y in r.y as usize..r.bottom() as usize {
                let row = y * stride;
                dst_bytes[row + x0..row + x1].copy_from_slice(&src_bytes[row + x0..row + x1]);
            }
        }
        dst.seq = src.seq;
        true
    }

    pub fn canvas(&mut self, index: usize) -> &mut [u8] {
        let slot = &mut self.slots[index];
        slot.buffer.canvas(&mut slot.pool).unwrap_or(&mut [])
    }

    pub fn buffer(&self, index: usize) -> &Buffer {
        &self.slots[index].buffer
    }

    /// Record that `index` was committed with `damage`.
    pub fn committed(&mut self, index: usize, damage: &[Rect]) {
        self.seq += 1;
        self.history.push(self.seq, damage);
        self.slots[index].seq = Some(self.seq);
        self.newest = Some(index);
    }

    /// A draw into `index` was not committed. If the app `touched` the
    /// pixels, the slot's contents are unknown and are forgotten; otherwise
    /// the slot still holds what it held.
    pub fn discard(&mut self, index: usize, touched: bool) {
        match discard_choice(self.newest, index, touched) {
            Discard::Keep => {}
            Discard::Forget(index) => self.slots[index].seq = None,
            Discard::ForgetAll => {
                for s in &mut self.slots {
                    s.seq = None;
                }
                self.newest = None;
                self.history.clear();
            }
        }
    }
}

fn pair_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (l, r) = v.split_at_mut(b);
        (&mut l[a], &mut r[0])
    } else {
        let (l, r) = v.split_at_mut(a);
        (&mut r[0], &mut l[b])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(busy: bool, seq: Option<u64>) -> SlotView {
        SlotView { busy, seq }
    }

    #[test]
    fn reuses_released_newest_slot() {
        let slots = [view(false, Some(3)), view(false, Some(4))];
        assert_eq!(choose_slot(&slots, Some(4), 3), SlotChoice::Current(1));
    }

    #[test]
    fn falls_back_to_most_recent_free_slot() {
        let slots = [
            view(false, Some(1)),
            view(true, Some(4)),
            view(false, Some(3)),
        ];
        assert_eq!(choose_slot(&slots, Some(4), 3), SlotChoice::Stale(2));
        let slots = [view(false, None), view(true, Some(4))];
        assert_eq!(choose_slot(&slots, Some(4), 3), SlotChoice::Stale(0));
    }

    #[test]
    fn allocates_then_waits_when_all_held() {
        assert_eq!(choose_slot(&[], None, 3), SlotChoice::Allocate);
        let held = [view(true, Some(1)), view(true, Some(2))];
        assert_eq!(choose_slot(&held, Some(2), 3), SlotChoice::Allocate);
        let held = [view(true, Some(1)), view(true, Some(2)), view(true, None)];
        assert_eq!(choose_slot(&held, Some(2), 3), SlotChoice::Wait);
    }

    #[test]
    fn steady_state_never_allocates() {
        // Simulate a compositor that releases each buffer before the next
        // frame: the same slot is chosen every time.
        let mut slots = vec![];
        let mut newest = None;
        for seq in 1..=100u64 {
            let idx = match choose_slot(&slots, newest, 3) {
                SlotChoice::Allocate => {
                    slots.push(view(false, None));
                    slots.len() - 1
                }
                SlotChoice::Current(i) | SlotChoice::Stale(i) => i,
                SlotChoice::Wait => unreachable!(),
            };
            slots[idx].seq = Some(seq);
            newest = Some(seq);
        }
        assert_eq!(slots.len(), 1);
    }

    #[test]
    fn a_no_op_draw_only_forgets_what_was_written() {
        // Took the buffer, committed nothing: its contents are unknown, and
        // it held the newest frame, so every slot is stale.
        assert_eq!(discard_choice(Some(1), 1, true), Discard::ForgetAll);
        assert_eq!(discard_choice(Some(0), 1, true), Discard::Forget(1));
        // Wrote nothing: the slot is still the newest frame and the next
        // draw stays partial.
        assert_eq!(discard_choice(Some(1), 1, false), Discard::Keep);
    }

    #[test]
    fn history_since() {
        let mut h = DamageHistory::default();
        assert_eq!(h.since(Some(0), 100, 100), None);
        h.push(1, &[Rect::new(0, 0, 10, 10)]);
        h.push(2, &[Rect::new(50, 50, 10, 10)]);
        h.push(3, &[Rect::new(10, 0, 10, 10)]);
        assert_eq!(h.since(Some(3), 100, 100), Some(vec![]));
        assert_eq!(
            h.since(Some(2), 100, 100),
            Some(vec![Rect::new(10, 0, 10, 10)])
        );
        let mut all = h.since(Some(0), 100, 100).unwrap();
        all.sort_by_key(|r| r.x);
        assert_eq!(
            all,
            vec![Rect::new(0, 0, 20, 10), Rect::new(50, 50, 10, 10)]
        );
        assert_eq!(h.since(None, 100, 100), None);
        for seq in 4..=20 {
            h.push(seq, &[]);
        }
        assert_eq!(h.since(Some(2), 100, 100), None);
        assert_eq!(h.since(Some(12), 100, 100), Some(vec![]));
    }
}
