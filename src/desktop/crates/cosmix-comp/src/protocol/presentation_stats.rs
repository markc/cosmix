//! Presentation statistics shared by windows and content sources: counters,
//! rings of the newest samples, percentiles and the missed-deadline rule.
//!
//! Stats follow content, not `wp_presentation` feedback, so every client is
//! measured whether or not it asks for feedback. An update is one buffer
//! published to the renderer (a window) or one revision (a content source).
//! A frame that shows a newer update than the last one shown presents it;
//! the updates it skipped count as discarded.
//!
//! All times are CLOCK_MONOTONIC microseconds.
// The readers (props, `comp.window.stats`) are the `bus` feature's.
#![cfg_attr(not(feature = "bus"), allow(dead_code))]

use std::collections::{HashMap, VecDeque};

#[cfg(feature = "bus")]
use serde_json::{Value, json};

/// Every ring keeps this many newest samples.
pub(crate) const STATS_RING: usize = 512;

/// Commit times kept per surface while they wait to be shown.
const PENDING_UPDATES: usize = 16;

/// Injected-input marks kept for content sources to match against.
const INPUT_MARKS: usize = 256;

/// The newest `STATS_RING` samples of one measurement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ring(VecDeque<u64>);

impl Ring {
    pub(crate) fn push(&mut self, value: u64) {
        if self.0.len() == STATS_RING {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    /// Nearest-rank percentile; `None` without samples.
    pub(crate) fn percentile(&self, percent: u64) -> Option<u64> {
        if self.0.is_empty() {
            return None;
        }
        let mut sorted = self.0.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        let rank = (percent as usize * sorted.len()).div_ceil(100).max(1);
        sorted.get(rank - 1).copied()
    }

    pub(crate) fn max(&self) -> Option<u64> {
        self.0.iter().copied().max()
    }

    /// The newest `count` samples, oldest first.
    pub(crate) fn newest(&self, count: usize) -> Vec<u64> {
        let skip = self.0.len().saturating_sub(count);
        self.0.iter().skip(skip).copied().collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// An injected input that a later presentation answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputMark {
    pub(crate) input_seq: u64,
    pub(crate) at_us: u64,
}

/// One presented update, as seen by the stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PresentSample {
    pub(crate) tv_us: u64,
    /// The output's fixed refresh; `None` when unknown (nested) or variable.
    pub(crate) refresh_us: Option<u64>,
    /// Updates superseded before any frame showed them.
    pub(crate) discarded: u64,
    /// When the presented update was committed, if known.
    pub(crate) committed_us: Option<u64>,
    /// When the oldest update this frame resolved was committed: from then
    /// on the client had something waiting for a vblank.
    pub(crate) pending_since_us: Option<u64>,
    /// The injected input this update answers (content sources), if known.
    pub(crate) answered_input_us: Option<u64>,
}

/// Counters and rings for one window or content source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PresentationStats {
    pub(crate) presented: u64,
    pub(crate) discarded: u64,
    pub(crate) last_presented_us: Option<u64>,
    pub(crate) intervals_us: Ring,
    pub(crate) commit_to_present_us: Ring,
    pub(crate) input_to_present_us: Ring,
    /// `None` until a frame with a known fixed refresh presents: an
    /// unmeasured count is not a perfect one.
    pub(crate) missed: Option<u64>,
    pub(crate) refresh_us: Option<u64>,
    /// When counting started (creation or the last reset).
    pub(crate) since_us: u64,
    /// The previous presentation while continuously shown; an interval is
    /// only measured between two of those.
    interval_anchor_us: Option<u64>,
    input_mark: Option<InputMark>,
}

impl PresentationStats {
    pub(crate) fn new(since_us: u64) -> Self {
        Self {
            since_us,
            ..Self::default()
        }
    }

    pub(crate) fn record_present(&mut self, sample: PresentSample) {
        self.presented += 1;
        self.discarded += sample.discarded;
        self.refresh_us = sample.refresh_us;
        if let Some(refresh) = sample.refresh_us.filter(|refresh| *refresh > 0) {
            let mut missed = self.missed.unwrap_or(0);
            if let Some(previous) = self.interval_anchor_us {
                missed += missed_vblanks(previous, sample.tv_us, refresh, sample.pending_since_us);
            }
            self.missed = Some(missed);
        }
        if let Some(previous) = self.interval_anchor_us {
            self.intervals_us
                .push(sample.tv_us.saturating_sub(previous));
        }
        if let Some(committed) = sample.committed_us {
            self.commit_to_present_us
                .push(sample.tv_us.saturating_sub(committed));
            // The first update committed after the input answers it.
            if let Some(mark) = self.input_mark
                && committed >= mark.at_us
            {
                self.input_to_present_us
                    .push(sample.tv_us.saturating_sub(mark.at_us));
                self.input_mark = None;
            }
        }
        if let Some(input) = sample.answered_input_us {
            self.input_to_present_us
                .push(sample.tv_us.saturating_sub(input));
        }
        self.interval_anchor_us = Some(sample.tv_us);
        self.last_presented_us = Some(sample.tv_us);
    }

    pub(crate) fn record_discarded(&mut self, count: u64) {
        self.discarded += count;
    }

    /// A frame did not show this window: the next presentation starts a new
    /// run instead of measuring the hidden gap as one long interval.
    pub(crate) fn hidden(&mut self) {
        self.interval_anchor_us = None;
    }

    /// Injected input was delivered to this window. Only the newest mark
    /// waits.
    pub(crate) fn mark_input(&mut self, mark: InputMark) {
        self.input_mark = Some(mark);
    }

    pub(crate) fn reset(&mut self, now_us: u64) {
        *self = Self::new(now_us);
    }

    /// The `*.presentation.*` leaves.
    pub(crate) fn leaves(&self) -> PresentationLeaves {
        PresentationLeaves {
            presented: self.presented,
            discarded: self.discarded,
            last_presented_us: self.last_presented_us,
            interval_p50_us: self.intervals_us.percentile(50),
            interval_p99_us: self.intervals_us.percentile(99),
            interval_max_us: self.intervals_us.max(),
            commit_to_present_p50_us: self.commit_to_present_us.percentile(50),
            commit_to_present_p99_us: self.commit_to_present_us.percentile(99),
            input_to_present_p50_us: self.input_to_present_us.percentile(50),
            input_to_present_p99_us: self.input_to_present_us.percentile(99),
            missed: self.missed,
            refresh_us: self.refresh_us,
            since_us: self.since_us,
        }
    }

    /// The `comp.window.stats` rings.
    #[cfg(feature = "bus")]
    pub(crate) fn samples(&self, count: usize) -> Value {
        json!({
            "intervals_us": self.intervals_us.newest(count),
            "commit_to_present_us": self.commit_to_present_us.newest(count),
            "input_to_present_us": self.input_to_present_us.newest(count),
        })
    }
}

/// Vblanks skipped between two presentations `refresh` apart, counted only
/// from the first one at which the client had an update waiting.
fn missed_vblanks(previous: u64, now: u64, refresh: u64, pending_since: Option<u64>) -> u64 {
    let Some(pending_since) = pending_since else {
        return 0;
    };
    let interval = now.saturating_sub(previous);
    let vblanks = (interval + refresh / 2) / refresh;
    if vblanks < 2 {
        return 0;
    }
    // Skipped vblanks are at previous + k·refresh for k in 1..vblanks.
    let first = pending_since
        .saturating_sub(previous)
        .div_ceil(refresh)
        .max(1);
    vblanks.saturating_sub(first)
}

/// The `windows.s<id>.presentation.*` leaves (sources add their costs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "bus", derive(serde::Serialize))]
pub(crate) struct PresentationLeaves {
    pub(crate) presented: u64,
    pub(crate) discarded: u64,
    pub(crate) last_presented_us: Option<u64>,
    pub(crate) interval_p50_us: Option<u64>,
    pub(crate) interval_p99_us: Option<u64>,
    pub(crate) interval_max_us: Option<u64>,
    pub(crate) commit_to_present_p50_us: Option<u64>,
    pub(crate) commit_to_present_p99_us: Option<u64>,
    pub(crate) input_to_present_p50_us: Option<u64>,
    pub(crate) input_to_present_p99_us: Option<u64>,
    pub(crate) missed: Option<u64>,
    pub(crate) refresh_us: Option<u64>,
    pub(crate) since_us: u64,
}

#[cfg(feature = "bus")]
impl PresentationLeaves {
    pub(crate) fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Per-surface bookkeeping between a published update and the frame that
/// shows it.
#[derive(Clone, Debug, Default)]
struct SurfaceUpdates {
    last_shown: u64,
    /// `(content_seq, committed_us)`, oldest first.
    pending: VecDeque<(u64, u64)>,
}

/// One window's stats, fenced by the role generation they were taken under.
#[derive(Clone, Debug)]
pub(crate) struct WindowStats {
    pub(crate) generation: u64,
    pub(crate) stats: PresentationStats,
}

/// One output's frame cadence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputStats {
    pub(crate) frames: u64,
    pub(crate) intervals_us: Ring,
    pub(crate) last_tv_us: Option<u64>,
    pub(crate) flags: u32,
    pub(crate) refresh_us: Option<u64>,
    pub(crate) since_us: u64,
}

impl OutputStats {
    fn new(since_us: u64) -> Self {
        Self {
            since_us,
            ..Self::default()
        }
    }

    pub(crate) fn record(&mut self, tv_us: u64, flags: u32, refresh_us: Option<u64>) {
        self.frames += 1;
        if let Some(previous) = self.last_tv_us {
            self.intervals_us.push(tv_us.saturating_sub(previous));
        }
        self.last_tv_us = Some(tv_us);
        self.flags = flags;
        self.refresh_us = refresh_us;
    }
}

/// What one frame did to one window, folded over its surfaces.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowFrame {
    presented: bool,
    discarded: u64,
    committed_us: Option<u64>,
    pending_since_us: Option<u64>,
    hidden: bool,
}

/// Every stats table the protocol thread keeps.
#[derive(Default)]
pub(crate) struct StatsRegistry {
    surfaces: HashMap<u64, SurfaceUpdates>,
    windows: HashMap<u64, WindowStats>,
    outputs: HashMap<String, OutputStats>,
    input_marks: VecDeque<InputMark>,
    /// Start of counting for rows without stats of their own.
    pub(crate) epoch_us: u64,
}

impl StatsRegistry {
    #[cfg(test)]
    pub(crate) fn new(epoch_us: u64) -> Self {
        Self {
            epoch_us,
            ..Self::default()
        }
    }

    /// A buffer became content `seq` of `surface`, which belongs to window
    /// `window` (if any) under `generation`.
    pub(crate) fn note_published(
        &mut self,
        surface: u64,
        window: Option<(u64, u64)>,
        seq: u64,
        now_us: u64,
    ) {
        let updates = self
            .surfaces
            .entry(surface)
            .or_insert_with(|| SurfaceUpdates {
                last_shown: seq.saturating_sub(1),
                pending: VecDeque::new(),
            });
        if updates.pending.len() == PENDING_UPDATES {
            updates.pending.pop_front();
        }
        updates.pending.push_back((seq, now_us));
        if let Some((window, generation)) = window {
            self.window_entry(window, generation, now_us);
        }
    }

    fn window_entry(&mut self, window: u64, generation: u64, now_us: u64) -> &mut WindowStats {
        let entry = self.windows.entry(window).or_insert_with(|| WindowStats {
            generation,
            stats: PresentationStats::new(now_us),
        });
        if entry.generation != generation {
            *entry = WindowStats {
                generation,
                stats: PresentationStats::new(now_us),
            };
        }
        entry
    }

    /// One surface in one frame. Returns the fold for its window.
    pub(crate) fn surface_frame(
        &mut self,
        surface: u64,
        is_root: bool,
        seq: u64,
        shown: bool,
        fold: &mut WindowFrame,
    ) {
        let Some(updates) = self.surfaces.get_mut(&surface) else {
            if is_root && !shown {
                fold.hidden = true;
            }
            return;
        };
        if !shown {
            // Updates this frame did not show cannot be timed any more; they
            // count as discarded once a later update is shown.
            updates.pending.retain(|(pending, _)| *pending > seq);
            if is_root {
                fold.hidden = true;
            }
            return;
        }
        if seq <= updates.last_shown {
            return;
        }
        fold.presented = true;
        fold.discarded += seq - updates.last_shown - 1;
        updates.last_shown = seq;
        while let Some((pending, committed)) = updates.pending.front().copied() {
            if pending > seq {
                break;
            }
            updates.pending.pop_front();
            fold.pending_since_us = Some(
                fold.pending_since_us
                    .map_or(committed, |since| since.min(committed)),
            );
            if pending == seq {
                fold.committed_us = Some(
                    fold.committed_us
                        .map_or(committed, |before| before.min(committed)),
                );
            }
        }
    }

    /// Apply one window's fold for a frame at `tv_us`.
    pub(crate) fn window_frame(
        &mut self,
        window: u64,
        generation: u64,
        fold: WindowFrame,
        tv_us: u64,
        refresh_us: Option<u64>,
    ) {
        let stats = &mut self.window_entry(window, generation, tv_us).stats;
        if fold.presented {
            stats.record_present(PresentSample {
                tv_us,
                refresh_us,
                discarded: fold.discarded,
                committed_us: fold.committed_us,
                pending_since_us: fold.pending_since_us,
                answered_input_us: None,
            });
        } else {
            stats.record_discarded(fold.discarded);
        }
        if fold.hidden {
            stats.hidden();
        }
    }

    pub(crate) fn output_frame(
        &mut self,
        output: &str,
        tv_us: u64,
        flags: u32,
        refresh_us: Option<u64>,
    ) {
        let epoch = self.epoch_us;
        self.outputs
            .entry(output.to_string())
            .or_insert_with(|| OutputStats::new(epoch))
            .record(tv_us, flags, refresh_us);
    }

    /// The surface is destroyed or took a new role.
    pub(crate) fn forget_surface(&mut self, surface: u64) {
        self.surfaces.remove(&surface);
        self.windows.remove(&surface);
    }

    pub(crate) fn window(&self, window: u64, generation: u64) -> Option<&PresentationStats> {
        self.windows
            .get(&window)
            .filter(|entry| entry.generation == generation)
            .map(|entry| &entry.stats)
    }

    pub(crate) fn output(&self, output: &str) -> Option<&OutputStats> {
        self.outputs.get(output)
    }

    /// Zero one window (its stats restart now).
    pub(crate) fn reset_window(&mut self, window: u64, generation: u64, now_us: u64) {
        self.window_entry(window, generation, now_us)
            .stats
            .reset(now_us);
    }

    /// Zero every window and output; rows without stats count from now.
    pub(crate) fn reset_all(&mut self, now_us: u64) {
        self.epoch_us = now_us;
        for entry in self.windows.values_mut() {
            entry.stats.reset(now_us);
        }
        for output in self.outputs.values_mut() {
            *output = OutputStats::new(now_us);
        }
    }

    /// Injected input reached `window` (if any). Content sources match the
    /// mark later by `input_seq`.
    pub(crate) fn mark_input(&mut self, window: Option<(u64, u64)>, input_seq: u64, at_us: u64) {
        let mark = InputMark { input_seq, at_us };
        if self.input_marks.len() == INPUT_MARKS {
            self.input_marks.pop_front();
        }
        self.input_marks.push_back(mark);
        if let Some((window, generation)) = window {
            self.window_entry(window, generation, at_us)
                .stats
                .mark_input(mark);
        }
    }

    pub(crate) fn input_mark(&self, input_seq: u64) -> Option<InputMark> {
        self.input_marks
            .iter()
            .rev()
            .find(|mark| mark.input_seq == input_seq)
            .copied()
    }
}

#[cfg(test)]
#[path = "presentation_stats_tests.rs"]
mod tests;
