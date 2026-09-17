//! `wp_presentation` bookkeeping: feedback taken at commit, resolved when the
//! renderer reports a presented frame, and the in-process content-source
//! ledger that is measured the same way.
//!
//! The ledgers are pure data structures, generic over [`Feedback`], so the
//! resolution rules are tested without a Wayland client. `WaylandState`
//! glue (take at commit, report handling, lifecycle discards) lives at the
//! bottom of this file.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::*;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, Refresh,
};

/// At most this many commits per surface wait for a presented frame. A
/// client that commits faster than frames are presented loses the oldest
/// (as `discarded`), never memory.
pub(crate) const MAX_PENDING_COMMITS: usize = 8;

/// One `wp_presentation_feedback` object, or a test fake. Dropping one
/// without resolving it would leave a client waiting forever, so every
/// path out of the ledger calls exactly one of `presented`/`discarded`.
pub(crate) trait Feedback {
    fn presented(self, frame: &PresentedFrame);
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
    fn presented(self, frame: &PresentedFrame) {
        match &frame.output {
            Some(output) => PresentationFeedbackCallback::presented(
                self,
                output,
                frame.time,
                frame.refresh,
                frame.seq,
                frame.flags,
            ),
            // `presented` must name an output; without one the honest
            // answer is that the update was not shown anywhere we can name.
            None => PresentationFeedbackCallback::discarded(self),
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

impl<F: Feedback> PresentationLedger<F> {
    /// Record the callbacks of one applied commit. `seq` is the surface's
    /// content sequence after the commit (a commit without a new buffer
    /// carries the sequence of the content it leaves on screen).
    pub(crate) fn take_on_commit(&mut self, id: SurfaceId, seq: u64, callbacks: Vec<F>) {
        if callbacks.is_empty() {
            return;
        }
        let queue = self.pending.entry(id).or_default();
        match queue.back_mut() {
            Some(last) if last.seq == seq => last.callbacks.extend(callbacks),
            _ => queue.push_back(PendingCommit { seq, callbacks }),
        }
        while queue.len() > MAX_PENDING_COMMITS {
            if let Some(oldest) = queue.pop_front() {
                let counters = self.counters.entry(id).or_default();
                for callback in oldest.callbacks {
                    counters.discarded += 1;
                    callback.discarded();
                }
            }
        }
    }

    /// Apply one renderer report for one surface. With `shown`, the newest
    /// commit at or below `commit_seq` is presented and older ones were
    /// superseded; without it, everything at or below `commit_seq` was not
    /// seen. Commits above `commit_seq` keep waiting.
    pub(crate) fn resolve(
        &mut self,
        id: SurfaceId,
        commit_seq: u64,
        shown: bool,
        frame: &PresentedFrame,
    ) -> usize {
        let Some(queue) = self.pending.get_mut(&id) else {
            return 0;
        };
        let ready = queue
            .iter()
            .take_while(|entry| entry.seq <= commit_seq)
            .count();
        if ready == 0 {
            return 0;
        }
        let resolved = queue.drain(..ready).collect::<Vec<_>>();
        if queue.is_empty() {
            self.pending.remove(&id);
        }
        let counters = self.counters.entry(id).or_default();
        let mut presented = 0;
        let last = resolved.len() - 1;
        for (index, entry) in resolved.into_iter().enumerate() {
            let present = shown && index == last;
            for callback in entry.callbacks {
                if present {
                    counters.presented += 1;
                    presented += 1;
                    callback.presented(frame);
                } else {
                    counters.discarded += 1;
                    callback.discarded();
                }
            }
            if present {
                counters.last_presented_us = u64::try_from(frame.time.as_micros()).ok();
            }
        }
        presented
    }

    /// Unmap, minimise, destroy, role change or lock: nothing pending for
    /// this surface can be shown as the content it was committed as.
    pub(crate) fn discard_surface(&mut self, id: SurfaceId) -> usize {
        let Some(queue) = self.pending.remove(&id) else {
            return 0;
        };
        let counters = self.counters.entry(id).or_default();
        let mut discarded = 0;
        for entry in queue {
            for callback in entry.callbacks {
                counters.discarded += 1;
                discarded += 1;
                callback.discarded();
            }
        }
        discarded
    }

    /// The surface is gone for good: drop its counters too.
    pub(crate) fn forget_surface(&mut self, id: SurfaceId) {
        self.discard_surface(id);
        self.counters.remove(&id);
    }

    pub(crate) fn pending_surfaces(&self) -> Vec<SurfaceId> {
        self.pending.keys().copied().collect()
    }

    pub(crate) fn pending_count(&self, id: SurfaceId) -> usize {
        self.pending.get(&id).map_or(0, |queue| {
            queue.iter().map(|entry| entry.callbacks.len()).sum()
        })
    }

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
    /// The newest commit whose content the frame could have shown.
    pub(crate) commit_seq: u64,
    /// Visible on this output with its texture prepared.
    pub(crate) shown: bool,
}

/// What one presented frame contained.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameContent {
    pub(crate) surfaces: Vec<FrameSurface>,
    pub(crate) sources: Vec<FrameSource>,
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
    registrations: HashMap<String, u64>,
}

impl SourceLedger {
    /// Returns the new registration counter for this id.
    pub(crate) fn register(&mut self, id: &str) -> u64 {
        let registration = self.registrations.entry(id.to_string()).or_default();
        *registration += 1;
        let registration = *registration;
        self.sources.insert(
            id.to_string(),
            SourceCounters {
                registration,
                ..SourceCounters::default()
            },
        );
        registration
    }

    /// Revisions never shown before unregistering count as discarded.
    pub(crate) fn unregister(&mut self, id: &str) -> Option<SourceCounters> {
        let mut counters = self.sources.remove(id)?;
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

    pub(crate) fn get(&self, id: &str) -> Option<&SourceCounters> {
        self.sources.get(id)
    }
}

/// Protocol-thread state for `wp_presentation`.
pub(crate) struct PresentationRuntime {
    /// `None` on a backend that cannot report presented frames yet: the
    /// global is not advertised there, so no client waits on it.
    pub(crate) _global: Option<smithay::wayland::presentation::PresentationState>,
    pub(crate) ledger: PresentationLedger<PresentationFeedbackCallback>,
    pub(crate) sources: SourceLedger,
    /// Surfaces committed in the transaction being applied, with what the
    /// commit path needs to judge whether the commit became content.
    pub(crate) applied_commits: Vec<AppliedCommit>,
}

pub(crate) struct AppliedCommit {
    surface: WlSurface,
    buffer_attached: bool,
    commit_count_before: Option<u64>,
}

impl WaylandState {
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
        let commit_count_before = self
            .surfaces
            .get(&surface.id())
            .map(|record| record.commit_count);
        self.presentation.applied_commits.push(AppliedCommit {
            surface: surface.clone(),
            buffer_attached,
            commit_count_before,
        });
    }

    /// After every surface in a transaction has applied (synchronised
    /// subsurfaces included): take each commit's feedback into the ledger.
    pub(super) fn take_presentation_commits(&mut self) {
        for commit in mem::take(&mut self.presentation.applied_commits) {
            if commit.surface.is_alive() {
                self.take_presentation_feedback(
                    &commit.surface,
                    commit.buffer_attached,
                    commit.commit_count_before,
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
        commit_count_before: Option<u64>,
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
        let accepted = record.filter(|record| {
            // A new buffer the commit path refused (retired before a configure
            // ack, rejected import) never became content.
            !buffer_attached
                || commit_count_before.is_some_and(|before| record.commit_count > before)
        });
        let Some(record) = accepted else {
            for callback in callbacks {
                callback.discarded();
            }
            return;
        };
        let (id, seq) = (record.id, record.commit_count);
        if record.mapped {
            self.presentation.ledger.take_on_commit(id, seq, callbacks);
        } else {
            for callback in callbacks {
                callback.discarded();
            }
        }
    }

    /// A lifecycle edge after which nothing pending can be shown as
    /// committed.
    pub(super) fn discard_presentation_feedback(&mut self, id: SurfaceId, reason: &'static str) {
        let discarded = self.presentation.ledger.discard_surface(id);
        if discarded > 0 {
            crate::frame_trace::event("comp_presentation_discarded", || {
                (id.0, 0, discard_reason_code(reason))
            });
        }
    }

    pub(super) fn frame_presented(&mut self, frame: PresentedFrame, content: FrameContent) {
        let frame = PresentedFrame {
            output: frame.output.or_else(|| self.backend.default_output()),
            ..frame
        };
        let lock_active = self.session_lock_active();
        let time_us = u64::try_from(frame.time.as_micros()).unwrap_or(u64::MAX);
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
            let presented = self.presentation.ledger.resolve(
                surface.id,
                surface.commit_seq,
                surface.shown && presentable,
                &frame,
            );
            if presented > 0 {
                crate::frame_trace::event("comp_presented", || {
                    (surface.id.0, surface.commit_seq, time_us)
                });
            }
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
                self.discard_presentation_feedback(id, "not_presentable");
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

    pub(super) fn content_source_unregistered(&mut self, id: &str) {
        if let Some(counters) = self.presentation.sources.unregister(id) {
            tracing::debug!(
                source = id,
                presented = counters.presented,
                discarded = counters.discarded,
                "content source unregistered"
            );
        }
    }
}

fn discard_reason_code(reason: &'static str) -> u64 {
    match reason {
        "unmap" => 1,
        "minimize" => 2,
        "destroy" => 3,
        "role" => 4,
        "lock" => 5,
        "not_presentable" => 6,
        _ => 0,
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod ledger_tests;
