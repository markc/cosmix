//! `wp_presentation` bookkeeping: feedback taken at commit, resolved when the
//! renderer reports a presented frame, and the in-process content-source
//! ledger that is measured the same way.
//!
//! The ledgers are pure data structures, generic over [`Feedback`], so the
//! resolution rules are tested without a Wayland client. `WaylandState`
//! glue (take at commit, report handling, lifecycle discards) lives at the
//! bottom of this file.
//!
//! Sequencing: a surface's `content_seq` advances only when the protocol
//! thread publishes a new buffer to the renderer, and the renderer reports
//! the `content_seq` each frame actually sampled. A commit's feedback is
//! presented only by a frame that sampled exactly that commit's content;
//! older commits it superseded are discarded.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::*;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, PresentationState, Refresh,
};

/// At most this many commits per surface wait for a presented frame. A
/// client that commits faster than frames are presented loses the oldest
/// (as `discarded`), never memory.
pub(crate) const MAX_PENDING_COMMITS: usize = 8;

/// One `wp_presentation_feedback` object, or a test fake. Dropping one
/// without resolving it would leave a client waiting forever, so every
/// path out of the ledger calls exactly one of `presented`/`discarded`.
pub(crate) trait Feedback {
    /// Returns whether the feedback was actually sent as presented (a frame
    /// without a nameable output can only be sent as discarded).
    fn presented(self, frame: &PresentedFrame) -> bool;
    fn discarded(self);
}

/// What a renderer proved about one presented frame on one output.
#[derive(Clone, Debug)]
pub(crate) struct PresentedFrame {
    pub(crate) output: Option<Output>,
    /// CLOCK_MONOTONIC.
    pub(crate) time: Duration,
    pub(crate) refresh: Refresh,
    pub(crate) seq: u64,
    pub(crate) flags: wp_presentation_feedback::Kind,
}

impl Feedback for PresentationFeedbackCallback {
    fn presented(self, frame: &PresentedFrame) -> bool {
        match &frame.output {
            Some(output) => {
                PresentationFeedbackCallback::presented(
                    self,
                    output,
                    frame.time,
                    frame.refresh,
                    frame.seq,
                    frame.flags,
                );
                true
            }
            // `presented` must name an output; without one the honest
            // answer is that the update was not shown anywhere we can name.
            None => {
                PresentationFeedbackCallback::discarded(self);
                false
            }
        }
    }

    fn discarded(self) {
        PresentationFeedbackCallback::discarded(self);
    }
}

/// Per-surface presentation counters (the stats surface builds on these).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PresentationCounters {
    pub(crate) presented: u64,
    pub(crate) discarded: u64,
    pub(crate) last_presented_us: Option<u64>,
}

/// What one ledger operation resolved: `(commit seq, callbacks)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub(crate) presented: Option<(u64, usize)>,
    pub(crate) discarded: Vec<(u64, usize)>,
}

struct PendingCommit<F> {
    seq: u64,
    callbacks: Vec<F>,
}

/// Feedback waiting for the frame that shows its commit.
pub(crate) struct PresentationLedger<F: Feedback> {
    pending: HashMap<SurfaceId, VecDeque<PendingCommit<F>>>,
    counters: HashMap<SurfaceId, PresentationCounters>,
}

impl<F: Feedback> Default for PresentationLedger<F> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            counters: HashMap::new(),
        }
    }
}

fn discard_entry<F: Feedback>(
    entry: PendingCommit<F>,
    counters: &mut PresentationCounters,
    resolution: &mut Resolution,
) {
    let count = entry.callbacks.len();
    for callback in entry.callbacks {
        callback.discarded();
    }
    counters.discarded += count as u64;
    if count > 0 {
        resolution.discarded.push((entry.seq, count));
    }
}

impl<F: Feedback> PresentationLedger<F> {
    /// Record the callbacks of one applied commit. `seq` is the surface's
    /// content sequence after the commit (a commit without a new buffer
    /// carries the sequence of the content it leaves on screen). Returns
    /// what the per-surface cap pushed out.
    pub(crate) fn take_on_commit(
        &mut self,
        id: SurfaceId,
        seq: u64,
        callbacks: Vec<F>,
    ) -> Resolution {
        let mut resolution = Resolution::default();
        if callbacks.is_empty() {
            return resolution;
        }
        let queue = self.pending.entry(id).or_default();
        match queue.back_mut() {
            Some(last) if last.seq == seq => last.callbacks.extend(callbacks),
            _ => queue.push_back(PendingCommit { seq, callbacks }),
        }
        let counters = self.counters.entry(id).or_default();
        while queue.len() > MAX_PENDING_COMMITS {
            if let Some(oldest) = queue.pop_front() {
                discard_entry(oldest, counters, &mut resolution);
            }
        }
        resolution
    }

    /// Apply one renderer report for one surface. With `shown`, commits
    /// below `commit_seq` were superseded before any frame showed them and
    /// are discarded, and the commit at exactly `commit_seq` is presented.
    /// Without it, everything at or below `commit_seq` was not seen.
    /// Commits above `commit_seq` keep waiting.
    pub(crate) fn resolve(
        &mut self,
        id: SurfaceId,
        commit_seq: u64,
        shown: bool,
        frame: &PresentedFrame,
    ) -> Resolution {
        let mut resolution = Resolution::default();
        let Some(queue) = self.pending.get_mut(&id) else {
            return resolution;
        };
        let ready = queue
            .iter()
            .take_while(|entry| entry.seq <= commit_seq)
            .count();
        if ready == 0 {
            return resolution;
        }
        let resolved = queue.drain(..ready).collect::<Vec<_>>();
        if queue.is_empty() {
            self.pending.remove(&id);
        }
        let counters = self.counters.entry(id).or_default();
        for entry in resolved {
            if !(shown && entry.seq == commit_seq) {
                discard_entry(entry, counters, &mut resolution);
                continue;
            }
            let count = entry.callbacks.len();
            let mut sent = 0;
            for callback in entry.callbacks {
                if callback.presented(frame) {
                    sent += 1;
                }
            }
            counters.presented += sent as u64;
            counters.discarded += (count - sent) as u64;
            if sent > 0 {
                counters.last_presented_us = u64::try_from(frame.time.as_micros()).ok();
                resolution.presented = Some((entry.seq, sent));
            }
            if sent < count {
                resolution.discarded.push((entry.seq, count - sent));
            }
        }
        resolution
    }

    /// The renderer refused this commit's buffer: its feedback (and that of
    /// bufferless commits that inherited its sequence) is never shown.
    pub(crate) fn discard_commit(&mut self, id: SurfaceId, seq: u64) -> Resolution {
        let mut resolution = Resolution::default();
        let Some(queue) = self.pending.get_mut(&id) else {
            return resolution;
        };
        let counters = self.counters.entry(id).or_default();
        let (refused, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(queue)
            .into_iter()
            .partition(|entry| entry.seq == seq);
        *queue = kept;
        for entry in refused {
            discard_entry(entry, counters, &mut resolution);
        }
        if queue.is_empty() {
            self.pending.remove(&id);
        }
        resolution
    }

    /// Unmap, minimise, destroy, role change: nothing pending for this
    /// surface can be shown as the content it was committed as.
    pub(crate) fn discard_surface(&mut self, id: SurfaceId) -> Resolution {
        let mut resolution = Resolution::default();
        let Some(queue) = self.pending.remove(&id) else {
            return resolution;
        };
        let counters = self.counters.entry(id).or_default();
        for entry in queue {
            discard_entry(entry, counters, &mut resolution);
        }
        resolution
    }

    /// The surface is gone for good: drop its counters too.
    pub(crate) fn forget_counters(&mut self, id: SurfaceId) {
        self.counters.remove(&id);
    }

    pub(crate) fn pending_surfaces(&self) -> Vec<SurfaceId> {
        self.pending.keys().copied().collect()
    }

    // Read by the stats surface (step 6) and the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending_count(&self, id: SurfaceId) -> usize {
        self.pending.get(&id).map_or(0, |queue| {
            queue.iter().map(|entry| entry.callbacks.len()).sum()
        })
    }

    // Read by the stats surface (step 6) and the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn counters(&self, id: SurfaceId) -> PresentationCounters {
        self.counters.get(&id).copied().unwrap_or_default()
    }
}

impl<F: Feedback> Drop for PresentationLedger<F> {
    fn drop(&mut self) {
        for (_, queue) in self.pending.drain() {
            for entry in queue {
                for callback in entry.callbacks {
                    callback.discarded();
                }
            }
        }
    }
}

/// One content source's state in a renderer report (see
/// `crate::content_source`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameSource {
    pub(crate) id: String,
    pub(crate) revision: u64,
    pub(crate) shown: bool,
    pub(crate) upload_bytes: u64,
    pub(crate) damage_px: u64,
    pub(crate) consumed_input: Option<u64>,
}

/// One client surface's state in a renderer report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSurface {
    pub(crate) id: SurfaceId,
    /// With `shown`: the content sequence this frame sampled. Without it:
    /// everything at or below this sequence was not seen.
    pub(crate) commit_seq: u64,
    /// Visible, on the output, and sampling that commit's content.
    pub(crate) shown: bool,
}

/// What one presented frame contained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameContent {
    pub(crate) surfaces: Vec<FrameSurface>,
    pub(crate) sources: Vec<FrameSource>,
    /// Commits whose buffer the renderer failed to import after accepting
    /// them: `(surface, content sequence)`.
    pub(crate) refused: Vec<(SurfaceId, u64)>,
}

/// Accounting for one registered content source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceCounters {
    pub(crate) registration: u64,
    pub(crate) revision: u64,
    pub(crate) presented: u64,
    pub(crate) discarded: u64,
    pub(crate) last_presented_revision: u64,
    pub(crate) last_presented_us: Option<u64>,
    pub(crate) upload_bytes_total: u64,
    pub(crate) damage_px_total: u64,
    pub(crate) frames: u64,
}

/// Content sources have no protocol callbacks; a revision counter stands in
/// for commits and the same presented/discarded rules apply.
#[derive(Default)]
pub(crate) struct SourceLedger {
    sources: HashMap<String, SourceCounters>,
    /// One counter for every registration of any id: a registration number
    /// never repeats, so it fences a stale request without per-id history.
    registrations: u64,
}

impl SourceLedger {
    /// Returns the new registration number.
    pub(crate) fn register(&mut self, id: &str) -> u64 {
        self.registrations += 1;
        self.sources.insert(
            id.to_string(),
            SourceCounters {
                registration: self.registrations,
                ..SourceCounters::default()
            },
        );
        self.registrations
    }

    /// `revision` is the newest revision the plugin wrote, reported or not;
    /// every revision after the last presented one counts as discarded.
    pub(crate) fn unregister(&mut self, id: &str, revision: u64) -> Option<SourceCounters> {
        let mut counters = self.sources.remove(id)?;
        counters.revision = counters.revision.max(revision);
        counters.discarded += counters
            .revision
            .saturating_sub(counters.last_presented_revision);
        Some(counters)
    }

    pub(crate) fn resolve(&mut self, source: &FrameSource, time: Duration) {
        let Some(counters) = self.sources.get_mut(&source.id) else {
            return;
        };
        counters.frames += 1;
        counters.upload_bytes_total = counters
            .upload_bytes_total
            .saturating_add(source.upload_bytes);
        counters.damage_px_total = counters.damage_px_total.saturating_add(source.damage_px);
        counters.revision = counters.revision.max(source.revision);
        if source.shown && source.revision > counters.last_presented_revision {
            counters.discarded += source.revision - counters.last_presented_revision - 1;
            counters.presented += 1;
            counters.last_presented_revision = source.revision;
            counters.last_presented_us = u64::try_from(time.as_micros()).ok();
        }
    }

    // Read by the stats surface (step 6) and the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn get(&self, id: &str) -> Option<&SourceCounters> {
        self.sources.get(id)
    }

    // Read by the tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.sources.len()
    }
}

/// Protocol-thread state for `wp_presentation`.
#[derive(Default)]
pub(crate) struct PresentationRuntime {
    /// Created only once a backend has wired a frame reporter
    /// ([`WaylandState::enable_presentation`]), so no client can wait on
    /// feedback nothing will ever resolve.
    global: Option<PresentationState>,
    pub(crate) ledger: PresentationLedger<PresentationFeedbackCallback>,
    pub(crate) sources: SourceLedger,
    /// Surfaces committed in the transaction being applied, with what the
    /// commit path needs to judge whether the commit became content.
    applied_commits: Vec<AppliedCommit>,
}

struct AppliedCommit {
    surface: WlSurface,
    buffer_attached: bool,
    content_seq_before: Option<u64>,
}

/// `frame_trace` reason codes (the `aux` field of
/// `comp_presentation_discarded`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum DiscardReason {
    Unmap = 1,
    Minimize = 2,
    Destroy = 3,
    Role = 4,
    Superseded = 5,
    NotPresentable = 6,
    Refused = 7,
    Overflow = 8,
    NoFrame = 9,
}

fn trace_discards(id: SurfaceId, resolution: &Resolution, reason: DiscardReason) {
    for (seq, _) in &resolution.discarded {
        crate::frame_trace::event("comp_presentation_discarded", || {
            (id.0, *seq, reason as u64)
        });
    }
}

impl WaylandState {
    /// Advertise `wp_presentation`. Called when a backend's frame reporter
    /// is wired; idempotent.
    pub(crate) fn enable_presentation(&mut self) {
        if self.presentation.global.is_none() {
            self.presentation.global = Some(PresentationState::new::<WaylandState>(
                &self.display_handle,
                libc::CLOCK_MONOTONIC as u32,
            ));
        }
    }

    /// First thing in the commit hook, before the commit path consumes the
    /// buffer: remember what this commit tried to do.
    pub(super) fn note_presentation_commit(&mut self, surface: &WlSurface) {
        let buffer_attached = compositor::with_states(surface, |states| {
            matches!(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer,
                Some(BufferAssignment::NewBuffer(_))
            )
        });
        let content_seq_before = self
            .surfaces
            .get(&surface.id())
            .map(|record| record.content_seq);
        self.presentation.applied_commits.push(AppliedCommit {
            surface: surface.clone(),
            buffer_attached,
            content_seq_before,
        });
    }

    /// After every surface in a transaction has applied (Smithay runs every
    /// surface's commit hook, synchronised subsurfaces included, before
    /// `transaction_applied`): take each commit's feedback into the ledger.
    pub(super) fn take_presentation_commits(&mut self) {
        for commit in mem::take(&mut self.presentation.applied_commits) {
            if commit.surface.is_alive() {
                self.take_presentation_feedback(
                    &commit.surface,
                    commit.buffer_attached,
                    commit.content_seq_before,
                );
            }
        }
    }

    /// Move one applied commit's feedback out of Smithay's cache (a later
    /// commit would otherwise discard it while this commit is still in
    /// flight) into the ledger.
    fn take_presentation_feedback(
        &mut self,
        surface: &WlSurface,
        buffer_attached: bool,
        content_seq_before: Option<u64>,
    ) {
        let callbacks = compositor::with_states(surface, |states| {
            mem::take(
                &mut states
                    .cached_state
                    .get::<PresentationFeedbackCachedState>()
                    .current()
                    .callbacks,
            )
        });
        if callbacks.is_empty() {
            return;
        }
        let record = self
            .surfaces
            .get(&surface.id())
            .filter(|record| !matches!(record.role, SurfaceRole::Dormant(_)));
        // A commit becomes content only when it is mapped and, if it
        // attached a buffer, that buffer was published to the renderer
        // (`content_seq` advanced). Everything else (no scene record, a
        // cursor, a buffer retired before its configure ack or refused by
        // the commit path, an X11 surface before its map) is discarded now.
        let accepted = record.filter(|record| {
            record.mapped
                && (!buffer_attached
                    || content_seq_before.is_some_and(|before| record.content_seq > before))
        });
        let Some(record) = accepted else {
            let (id, seq) =
                record.map_or((SurfaceId(0), 0), |record| (record.id, record.content_seq));
            let count = callbacks.len();
            for callback in callbacks {
                callback.discarded();
            }
            if count > 0 {
                crate::frame_trace::event("comp_presentation_discarded", || {
                    (id.0, seq, DiscardReason::Refused as u64)
                });
            }
            return;
        };
        let (id, seq) = (record.id, record.content_seq);
        let overflow = self.presentation.ledger.take_on_commit(id, seq, callbacks);
        trace_discards(id, &overflow, DiscardReason::Overflow);
    }

    /// A lifecycle edge after which nothing pending can be shown as
    /// committed.
    pub(super) fn discard_presentation_feedback(&mut self, id: SurfaceId, reason: DiscardReason) {
        let resolution = self.presentation.ledger.discard_surface(id);
        trace_discards(id, &resolution, reason);
    }

    /// The surface is destroyed: discard what waits, then drop its counters.
    pub(super) fn forget_presentation_surface(&mut self, id: SurfaceId) {
        self.discard_presentation_feedback(id, DiscardReason::Destroy);
        self.presentation.ledger.forget_counters(id);
    }

    pub(super) fn commit_refused(&mut self, id: SurfaceId, seq: u64) {
        let resolution = self.presentation.ledger.discard_commit(id, seq);
        trace_discards(id, &resolution, DiscardReason::Refused);
    }

    /// Session lock needs no discard of its own: a report during the lock
    /// treats every surface the lock hides as not shown.
    pub(super) fn frame_presented(&mut self, frame: PresentedFrame, content: FrameContent) {
        let frame = PresentedFrame {
            output: frame.output.or_else(|| self.backend.default_output()),
            ..frame
        };
        let lock_active = self.session_lock_active();
        let time_us = u64::try_from(frame.time.as_micros()).unwrap_or(u64::MAX);
        for (id, seq) in &content.refused {
            self.commit_refused(*id, *seq);
        }
        let mut reported = HashSet::new();
        for surface in &content.surfaces {
            reported.insert(surface.id);
            let presentable = self
                .surface_objects
                .get(&surface.id)
                .and_then(|object| self.surfaces.get(object))
                .is_some_and(|record| {
                    record.mapped
                        && !record.minimized
                        && (!lock_active || self.surface_is_session_presentable(record))
                });
            let shown = surface.shown && presentable;
            let resolution =
                self.presentation
                    .ledger
                    .resolve(surface.id, surface.commit_seq, shown, &frame);
            if let Some((seq, _)) = resolution.presented {
                crate::frame_trace::event("comp_presented", || (surface.id.0, seq, time_us));
            }
            trace_discards(
                surface.id,
                &resolution,
                if shown {
                    DiscardReason::Superseded
                } else {
                    DiscardReason::NoFrame
                },
            );
        }
        // A surface the renderer did not list and can never present will not
        // be listed later either; one it can present is still in flight.
        for id in self.presentation.ledger.pending_surfaces() {
            if reported.contains(&id) {
                continue;
            }
            let presentable = self
                .surface_objects
                .get(&id)
                .and_then(|object| self.surfaces.get(object))
                .is_some_and(|record| self.surface_is_renderer_presentable(record));
            if !presentable {
                self.discard_presentation_feedback(id, DiscardReason::NotPresentable);
            }
        }
        for source in &content.sources {
            self.presentation.sources.resolve(source, frame.time);
        }
    }

    pub(super) fn content_source_registered(&mut self, id: &str) {
        let registration = self.presentation.sources.register(id);
        tracing::debug!(source = id, registration, "content source registered");
    }

    pub(super) fn content_source_unregistered(&mut self, id: &str, revision: u64) {
        if let Some(counters) = self.presentation.sources.unregister(id, revision) {
            tracing::debug!(
                source = id,
                presented = counters.presented,
                discarded = counters.discarded,
                "content source unregistered"
            );
        }
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod ledger_tests;
