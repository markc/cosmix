//! Presentation statistics shared by windows and content sources: counters,
//! rings of the newest samples, percentiles and the missed-deadline rule.
//!
//! Stats follow content, not `wp_presentation` feedback, so every client is
//! measured whether or not it asks for feedback. An update is one buffer
//! published to the renderer (a window) or one revision (a content source).
//! A frame that shows a newer update than the last one shown presents it;
//! the updates it skipped count as discarded. Updates are assumed to be
//! numbered +1 each (a surface's `content_seq`, a source's revision); that
//! is not checked, so a jump counts every skipped number as discarded
//! (saturating).
//!
//! All times are CLOCK_MONOTONIC microseconds.
//!
//! Single output: a frame report names one output and a surface is either
//! shown by it or not. Folds are per window per report, so a per-output
//! split only needs the report's output added to the fold key.
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

/// An injected input older than this is not paired with a presentation: a
/// window that did not react within a second did not react to it.
pub(crate) const INPUT_MARK_TTL_US: u64 = 1_000_000;

/// A frame reported this far before the last reset was in flight across it
/// and is dropped. Anything older cannot be a reset race: its clock does
/// not share `monotonic_us`'s base, so it is counted (and logged) instead
/// of silently freezing the row.
const PRE_RESET_BOUND_US: u64 = 10_000_000;

/// The newest `STATS_RING` samples of one measurement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ring(VecDeque<u64>);

/// Nearest-rank p50/p99 and the maximum of a ring, from one sort.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RingSummary {
    pub(crate) p50: Option<u64>,
    pub(crate) p99: Option<u64>,
    pub(crate) max: Option<u64>,
}

impl Ring {
    pub(crate) fn push(&mut self, value: u64) {
        if self.0.len() == STATS_RING {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    pub(crate) fn summary(&self) -> RingSummary {
        if self.0.is_empty() {
            return RingSummary::default();
        }
        let mut sorted = self.0.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        let rank = |percent: usize| {
            let rank = (percent * sorted.len()).div_ceil(100).max(1);
            sorted[rank - 1]
        };
        RingSummary {
            p50: Some(rank(50)),
            p99: Some(rank(99)),
            max: sorted.last().copied(),
        }
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
    pub(crate) injected_at_us: u64,
}

impl InputMark {
    /// Whether an update presented (or committed) at `at_us` can still be
    /// the answer to this input.
    pub(crate) fn live_at(self, at_us: u64) -> bool {
        at_us.saturating_sub(self.injected_at_us) <= INPUT_MARK_TTL_US
    }
}

/// One presented update, as seen by the stats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PresentSample {
    pub(crate) tv_us: u64,
    /// The output's fixed refresh; `None` when unknown (nested) or variable.
    pub(crate) refresh_us: Option<u64>,
    /// Updates superseded before any frame showed them.
    pub(crate) discarded: u64,
    /// When the presented update was published to the renderer, if known.
    pub(crate) committed_us: Option<u64>,
    /// When the oldest update this frame resolved was published: from then
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
    /// Null while the newest presentation's refresh is unknown or variable:
    /// an unmeasured count is not a perfect one.
    pub(crate) missed: Option<u64>,
    pub(crate) refresh_us: Option<u64>,
    /// When counting started (creation or the last reset).
    pub(crate) since_us: u64,
    /// Frames counted although they predate `since_us` by more than the
    /// bound: their clock base is not ours.
    pub(crate) clock_base_mismatches: u64,
    logged_pre_reset: bool,
    logged_clock_mismatch: bool,
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

    /// Whether a frame timed `tv_us` counts. A frame from just before the
    /// last reset was in flight across it; one from far before it is a
    /// different clock base, which must not freeze the row silently.
    fn counts(&mut self, tv_us: u64) -> bool {
        if tv_us >= self.since_us {
            return true;
        }
        if self.since_us - tv_us <= PRE_RESET_BOUND_US {
            if !self.logged_pre_reset {
                self.logged_pre_reset = true;
                tracing::debug!(
                    tv_us,
                    since_us = self.since_us,
                    "presentation stats dropped a frame reported after a reset"
                );
            }
            return false;
        }
        self.clock_base_mismatches = self.clock_base_mismatches.saturating_add(1);
        if !self.logged_clock_mismatch {
            self.logged_clock_mismatch = true;
            tracing::warn!(
                tv_us,
                since_us = self.since_us,
                "presented frame is far older than the last reset: counting it, \
                 its timestamps do not share CLOCK_MONOTONIC with the compositor"
            );
        }
        true
    }

    pub(crate) fn record_present(&mut self, sample: PresentSample) {
        if !self.counts(sample.tv_us) {
            return;
        }
        self.presented = self.presented.saturating_add(1);
        self.discarded = self.discarded.saturating_add(sample.discarded);
        self.refresh_us = sample.refresh_us;
        match sample.refresh_us.filter(|refresh| *refresh > 0) {
            Some(refresh) => {
                let mut missed = self.missed.unwrap_or(0);
                if let Some(previous) = self.interval_anchor_us {
                    missed = missed.saturating_add(missed_vblanks(
                        previous,
                        sample.tv_us,
                        refresh,
                        sample.pending_since_us,
                    ));
                }
                self.missed = Some(missed);
            }
            None => self.missed = None,
        }
        if let Some(previous) = self.interval_anchor_us {
            self.intervals_us
                .push(sample.tv_us.saturating_sub(previous));
        }
        if let Some(committed) = sample.committed_us {
            self.commit_to_present_us
                .push(sample.tv_us.saturating_sub(committed));
            // The first update committed after the input answers it. The
            // age that matters is the client's: a slow present must not
            // lose a sample the client answered promptly.
            if let Some(mark) = self.input_mark
                && !mark.live_at(committed)
            {
                self.input_mark = None;
            }
            if let Some(mark) = self.input_mark
                && committed >= mark.injected_at_us
            {
                self.input_to_present_us
                    .push(sample.tv_us.saturating_sub(mark.injected_at_us));
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

    /// Updates this frame will never show. `tv_us` is the frame's time when
    /// there is one, so a frame reported after a reset is dropped whole
    /// instead of leaving its discards behind.
    pub(crate) fn record_discarded(&mut self, count: u64, tv_us: Option<u64>) {
        if tv_us.is_some_and(|tv_us| !self.counts(tv_us)) {
            return;
        }
        self.discarded = self.discarded.saturating_add(count);
    }

    /// A frame did not show this window: the next presentation starts a new
    /// run instead of measuring the hidden gap as one long interval, and an
    /// input it was waiting to answer is dropped.
    pub(crate) fn hidden(&mut self) {
        self.interval_anchor_us = None;
        self.input_mark = None;
    }

    /// A frame report that did not list this window at all. The run breaks
    /// (nothing proves it was on screen), but an injected input keeps
    /// waiting: a window mapped a frame ago is simply not in the renderer's
    /// list yet, and the mark's own age still bounds it.
    pub(crate) fn unlisted(&mut self) {
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

    /// The `*.presentation.*` leaves: one sort per ring.
    pub(crate) fn leaves(&self) -> PresentationLeaves {
        let intervals = self.intervals_us.summary();
        let commit = self.commit_to_present_us.summary();
        let input = self.input_to_present_us.summary();
        PresentationLeaves {
            presented: self.presented,
            discarded: self.discarded,
            last_presented_us: self.last_presented_us,
            interval_p50_us: intervals.p50,
            interval_p99_us: intervals.p99,
            interval_max_us: intervals.max,
            commit_to_present_p50_us: commit.p50,
            commit_to_present_p99_us: commit.p99,
            input_to_present_p50_us: input.p50,
            input_to_present_p99_us: input.p99,
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
    let vblanks = interval.saturating_add(refresh / 2) / refresh;
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
    /// The window the surface belonged to when it last published.
    window: Option<u64>,
    /// The newest update a frame showed, or one a frame hid (a hidden
    /// update is discarded then, as the feedback ledger does, and is not
    /// presented if the same content is shown again later).
    last_resolved: u64,
    /// `(content_seq, published_us)`, oldest first.
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
        if tv_us < self.since_us {
            return;
        }
        self.frames = self.frames.saturating_add(1);
        if let Some(previous) = self.last_tv_us {
            self.intervals_us.push(tv_us.saturating_sub(previous));
        }
        self.last_tv_us = Some(tv_us);
        self.flags = flags;
        self.refresh_us = refresh_us;
    }
}

/// How one frame treated one surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceShown {
    /// Shown, sampling content `seq`.
    Shown,
    /// Visible, but its newest content is not sampled yet (a texture still
    /// uploading): a stall, not a hide. The run and its pending updates
    /// continue, so the eventual presentation measures the gap.
    Waiting,
    /// Not shown (hidden, minimised, off the output, locked away).
    Hidden,
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

impl WindowFrame {
    /// The renderer showed new content of this window in this frame.
    pub(crate) fn presented(&self) -> bool {
        self.presented
    }
}

/// Every stats table the protocol thread keeps.
#[derive(Default)]
pub(crate) struct StatsRegistry {
    surfaces: HashMap<u64, SurfaceUpdates>,
    windows: HashMap<u64, WindowStats>,
    outputs: HashMap<String, OutputStats>,
    input_marks: VecDeque<InputMark>,
    /// The newest input mark accepted; marks must arrive in increasing
    /// `input_seq` order, so a sequence number names one injection.
    last_input_seq: u64,
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
                last_resolved: seq.saturating_sub(1),
                ..SurfaceUpdates::default()
            });
        updates.window = window.map(|(window, _)| window);
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

    /// One surface in one frame, folded into its window's `fold`.
    pub(crate) fn surface_frame(
        &mut self,
        surface: u64,
        is_root: bool,
        seq: u64,
        state: SurfaceShown,
        fold: &mut WindowFrame,
    ) {
        if is_root && state == SurfaceShown::Hidden {
            fold.hidden = true;
        }
        let Some(updates) = self.surfaces.get_mut(&surface) else {
            return;
        };
        match state {
            SurfaceShown::Waiting => {}
            SurfaceShown::Hidden => {
                // As the feedback ledger does: everything up to `seq` was
                // not seen and is discarded now, so showing that content
                // again later is not a new presentation.
                if seq > updates.last_resolved {
                    fold.discarded = fold.discarded.saturating_add(seq - updates.last_resolved);
                    updates.last_resolved = seq;
                }
                updates.pending.retain(|(pending, _)| *pending > seq);
            }
            SurfaceShown::Shown => {
                if seq <= updates.last_resolved {
                    return;
                }
                fold.presented = true;
                fold.discarded = fold
                    .discarded
                    .saturating_add(seq - updates.last_resolved - 1);
                updates.last_resolved = seq;
                while let Some((pending, published)) = updates.pending.front().copied() {
                    if pending > seq {
                        break;
                    }
                    updates.pending.pop_front();
                    fold.pending_since_us = Some(
                        fold.pending_since_us
                            .map_or(published, |since| since.min(published)),
                    );
                    if pending == seq {
                        fold.committed_us = Some(
                            fold.committed_us
                                .map_or(published, |before| before.min(published)),
                        );
                    }
                }
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
            stats.record_discarded(fold.discarded, Some(tv_us));
        }
        if fold.hidden {
            stats.hidden();
        }
    }

    /// Windows the frame report did not list at all (unmapped, destroyed
    /// surfaces not yet forgotten, not in the scene) were not shown by it.
    pub(crate) fn hide_unlisted(&mut self, listed: impl Fn(u64) -> bool) {
        for (window, entry) in &mut self.windows {
            if !listed(*window) {
                entry.stats.unlisted();
            }
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

    /// Zero one window (its stats restart now). Updates published before
    /// the reset are not timed against it.
    pub(crate) fn reset_window(&mut self, window: u64, generation: u64, now_us: u64) {
        for updates in self.surfaces.values_mut() {
            if updates.window == Some(window) {
                updates.pending.clear();
            }
        }
        self.window_entry(window, generation, now_us)
            .stats
            .reset(now_us);
    }

    /// Zero every window and output; rows without stats count from now.
    pub(crate) fn reset_all(&mut self, now_us: u64) {
        self.epoch_us = now_us;
        for updates in self.surfaces.values_mut() {
            updates.pending.clear();
        }
        for entry in self.windows.values_mut() {
            entry.stats.reset(now_us);
        }
        for output in self.outputs.values_mut() {
            *output = OutputStats::new(now_us);
        }
        self.input_marks.clear();
        // The injection site's counter keeps running, so the next mark is
        // newer than anything this registry saw before the reset.
        self.last_input_seq = 0;
    }

    /// Injected input reached `window` (its root, if any). Content sources
    /// match the mark later by `input_seq`. Returns false (and records
    /// nothing) for an `input_seq` not newer than the last one accepted.
    pub(crate) fn mark_input(&mut self, window: Option<(u64, u64)>, mark: InputMark) -> bool {
        if mark.input_seq <= self.last_input_seq {
            return false;
        }
        self.last_input_seq = mark.input_seq;
        while self.input_marks.len() >= INPUT_MARKS
            || self
                .input_marks
                .front()
                .is_some_and(|oldest| !oldest.live_at(mark.injected_at_us))
        {
            self.input_marks.pop_front();
        }
        self.input_marks.push_back(mark);
        if let Some((window, generation)) = window {
            self.window_entry(window, generation, mark.injected_at_us)
                .stats
                .mark_input(mark);
        }
        true
    }

    /// The mark for `input_seq`, if it is still live at `at_us`.
    pub(crate) fn input_mark(&self, input_seq: u64, at_us: u64) -> Option<InputMark> {
        self.input_marks
            .iter()
            .rev()
            .find(|mark| mark.input_seq == input_seq)
            .copied()
            .filter(|mark| mark.live_at(at_us))
    }
}

#[cfg(test)]
#[path = "presentation_stats_tests.rs"]
mod tests;
