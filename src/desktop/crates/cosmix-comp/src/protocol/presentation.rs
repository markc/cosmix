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

use super::presentation_stats::{
    InputMark, PresentSample, PresentationLeaves, PresentationStats, Ring, StatsRegistry,
    SurfaceShown, WindowFrame,
};
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

    /// Commits in `from..=to` will never be shown (a refused buffer, the
    /// bufferless commits that inherited its sequence, and requests it
    /// superseded while they were pending).
    pub(crate) fn discard_range(&mut self, id: SurfaceId, from: u64, to: u64) -> Resolution {
        let mut resolution = Resolution::default();
        let Some(queue) = self.pending.get_mut(&id) else {
            return resolution;
        };
        let counters = self.counters.entry(id).or_default();
        let (refused, kept): (VecDeque<_>, VecDeque<_>) = std::mem::take(queue)
            .into_iter()
            .partition(|entry| (from..=to).contains(&entry.seq));
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
    /// When the newest revision was taken (CLOCK_MONOTONIC µs), if this
    /// report carries a new one.
    pub(crate) revised_us: Option<u64>,
    /// When the oldest revision since the previous report was taken.
    pub(crate) first_revised_us: Option<u64>,
}

impl FrameSource {
    /// Forget the costs a report already carried.
    pub(crate) fn clear_costs(&mut self) {
        self.upload_bytes = 0;
        self.damage_px = 0;
        self.consumed_input = None;
        self.revised_us = None;
        self.first_revised_us = None;
    }
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
    /// Not shown only because the newest content is not sampled yet (the
    /// surface is visible): the feedback ledger treats it as not shown, the
    /// statistics as a stall rather than a hide.
    pub(crate) waiting: bool,
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
    /// The output the plugin asked to be measured on (`None` = any).
    pub(crate) output: Option<String>,
    pub(crate) registered_at_us: u64,
    pub(crate) revision: u64,
    pub(crate) last_presented_revision: u64,
    pub(crate) stats: PresentationStats,
    pub(crate) upload_bytes_total: u64,
    pub(crate) damage_px_total: u64,
    /// Per reported frame, shown or not: the work was done either way.
    pub(crate) upload_bytes: Ring,
    pub(crate) damage_px: Ring,
    pub(crate) frames: u64,
    /// When the oldest revision not yet presented was written.
    pending_since_us: Option<u64>,
}

impl SourceCounters {
    fn reset(&mut self, now_us: u64) {
        *self = Self {
            registration: self.registration,
            output: self.output.take(),
            registered_at_us: self.registered_at_us,
            revision: self.revision,
            last_presented_revision: self.last_presented_revision,
            stats: PresentationStats::new(now_us),
            ..Self::default()
        };
    }

    /// The `sources.<id>.presentation.*` leaves.
    pub(crate) fn leaves(&self) -> SourcePresentationLeaves {
        let upload = self.upload_bytes.summary();
        let damage = self.damage_px.summary();
        SourcePresentationLeaves {
            common: self.stats.leaves(),
            upload_bytes_total: self.upload_bytes_total,
            damage_px_total: self.damage_px_total,
            upload_bytes_p50: upload.p50,
            upload_bytes_p99: upload.p99,
            damage_px_p50: damage.p50,
            damage_px_p99: damage.p99,
        }
    }

    /// The `comp.window.stats {source}` rings.
    #[cfg(feature = "bus")]
    pub(crate) fn samples(&self, count: usize) -> serde_json::Value {
        let mut samples = self.stats.samples(count);
        samples["upload_bytes"] = serde_json::json!(self.upload_bytes.newest(count));
        samples["damage_px"] = serde_json::json!(self.damage_px.newest(count));
        samples
    }
}

/// `sources.<id>.presentation.*`: the window leaves plus the costs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "bus", derive(serde::Serialize))]
pub(crate) struct SourcePresentationLeaves {
    #[cfg_attr(feature = "bus", serde(flatten))]
    pub(crate) common: PresentationLeaves,
    pub(crate) upload_bytes_total: u64,
    pub(crate) damage_px_total: u64,
    pub(crate) upload_bytes_p50: Option<u64>,
    pub(crate) upload_bytes_p99: Option<u64>,
    pub(crate) damage_px_p50: Option<u64>,
    pub(crate) damage_px_p99: Option<u64>,
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
    pub(crate) fn register(&mut self, id: &str, output: Option<String>, now_us: u64) -> u64 {
        self.registrations += 1;
        self.sources.insert(
            id.to_string(),
            SourceCounters {
                registration: self.registrations,
                output,
                registered_at_us: now_us,
                stats: PresentationStats::new(now_us),
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
        // Revisions are assumed to be +1 per update (not checked). No frame
        // time: an unregistration is not a frame report.
        counters.stats.record_discarded(
            counters
                .revision
                .saturating_sub(counters.last_presented_revision),
            None,
        );
        Some(counters)
    }

    /// One reported frame. `input_mark` finds when an injected input was
    /// delivered, for the update that says it answers it.
    pub(crate) fn resolve(
        &mut self,
        source: &FrameSource,
        tv_us: u64,
        refresh_us: Option<u64>,
        input_mark: impl Fn(u64, u64) -> Option<InputMark>,
    ) {
        let Some(counters) = self.sources.get_mut(&source.id) else {
            return;
        };
        counters.frames = counters.frames.saturating_add(1);
        counters.upload_bytes_total = counters
            .upload_bytes_total
            .saturating_add(source.upload_bytes);
        counters.damage_px_total = counters.damage_px_total.saturating_add(source.damage_px);
        counters.upload_bytes.push(source.upload_bytes);
        counters.damage_px.push(source.damage_px);
        counters.revision = counters.revision.max(source.revision);
        let unpresented = source.revision > counters.last_presented_revision;
        if unpresented && let Some(first) = source.first_revised_us {
            counters.pending_since_us = Some(
                counters
                    .pending_since_us
                    .map_or(first, |since| since.min(first)),
            );
        }
        if source.shown && unpresented {
            counters.stats.record_present(PresentSample {
                tv_us,
                refresh_us,
                discarded: source.revision - counters.last_presented_revision - 1,
                committed_us: source.revised_us,
                pending_since_us: counters.pending_since_us.take(),
                answered_input_us: source
                    .consumed_input
                    .and_then(|input_seq| input_mark(input_seq, tv_us))
                    .map(|mark| mark.injected_at_us),
            });
            counters.last_presented_revision = source.revision;
        } else if !source.shown {
            counters.stats.hidden();
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<&SourceCounters> {
        self.sources.get(id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &SourceCounters)> {
        self.sources.iter()
    }

    pub(crate) fn reset(&mut self, id: &str, now_us: u64) -> bool {
        self.sources
            .get_mut(id)
            .map(|counters| counters.reset(now_us))
            .is_some()
    }

    pub(crate) fn reset_all(&mut self, now_us: u64) {
        for counters in self.sources.values_mut() {
            counters.reset(now_us);
        }
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
    /// Window and output statistics (content-driven, feedback or not).
    pub(crate) stats: StatsRegistry,
    /// Per surface, the newest content sequence the renderer refused. A
    /// bufferless commit that still carries it can never be shown.
    refused: HashMap<SurfaceId, u64>,
    /// Surfaces committed in the transaction being applied, with what the
    /// commit path needs to judge whether the commit became content.
    applied_commits: Vec<AppliedCommit>,
}

struct AppliedCommit {
    surface: WlSurface,
    content_seq_before: Option<u64>,
}

/// One `wl_surface.commit`'s feedback, staged by the pre-commit hook before
/// Smithay caches the commit. A transaction can apply several cached
/// commits at once (a pending blocker, a synchronised parent); Smithay's own
/// feedback state keeps an interior commit's callbacks when a later commit
/// had none, so they would be presented with the later commit's content.
/// Staging keeps each commit's callbacks with what that commit did.
///
/// Commits cached under one role and applied after a role change enter the
/// ledger with the new role's content sequence (rare; the role change
/// itself discards what was already taken).
struct StagedCommit {
    callbacks: Vec<PresentationFeedbackCallback>,
    /// The commit attached or removed a buffer: every earlier commit's
    /// content is superseded.
    supersedes: bool,
    /// The commit attached a new buffer.
    new_buffer: bool,
}

impl Drop for StagedCommit {
    fn drop(&mut self) {
        // A surface destroyed with commits still cached.
        for callback in self.callbacks.drain(..) {
            callback.discarded();
        }
    }
}

/// Double-buffered staged feedback: the commits a transaction applied,
/// oldest first.
#[derive(Default)]
pub(super) struct StagedFeedback {
    commits: Vec<StagedCommit>,
}

impl Cacheable for StagedFeedback {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        mem::take(self)
    }

    fn merge_into(mut self, into: &mut Self, _dh: &DisplayHandle) {
        into.commits.append(&mut self.commits);
    }
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
    /// The surface left the current workspace (a switch or a move).
    Workspace = 10,
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
            self.presentation.stats.epoch_us = crate::frame_trace::monotonic_us();
            self.presentation.global = Some(PresentationState::new::<WaylandState>(
                &self.display_handle,
                libc::CLOCK_MONOTONIC as u32,
            ));
        }
    }

    /// First thing in the commit hook, before the commit path consumes the
    /// buffer: remember what this commit tried to do.
    pub(super) fn note_presentation_commit(&mut self, surface: &WlSurface) {
        let content_seq_before = self
            .surfaces
            .get(&surface.id())
            .map(|record| record.content_seq);
        self.presentation.applied_commits.push(AppliedCommit {
            surface: surface.clone(),
            content_seq_before,
        });
    }

    /// After every surface in a transaction has applied (Smithay runs every
    /// surface's commit hook, synchronised subsurfaces included, before
    /// `transaction_applied`): take each commit's feedback into the ledger.
    pub(super) fn take_presentation_commits(&mut self) {
        for commit in mem::take(&mut self.presentation.applied_commits) {
            if commit.surface.is_alive() {
                self.take_presentation_feedback(&commit.surface, commit.content_seq_before);
            }
        }
    }

    /// Move one applied commit's feedback out of Smithay's cache (a later
    /// commit would otherwise discard it while this commit is still in
    /// flight) into the ledger.
    fn take_presentation_feedback(&mut self, surface: &WlSurface, content_seq_before: Option<u64>) {
        let (commits, unstaged) = compositor::with_states(surface, |states| {
            (
                mem::take(
                    &mut states
                        .cached_state
                        .get::<StagedFeedback>()
                        .current()
                        .commits,
                ),
                mem::take(
                    &mut states
                        .cached_state
                        .get::<PresentationFeedbackCachedState>()
                        .current()
                        .callbacks,
                ),
            )
        });
        // Only the last commit that replaced the buffer (and the bufferless
        // commits after it) can be shown; earlier ones were superseded
        // inside the transaction. A NULL attach replaces the content with
        // nothing, so a transaction ending in one shows none of its
        // commits, whatever the record's map state is by the time the
        // unmap path runs.
        let last_content = commits.iter().rposition(|commit| commit.supersedes);
        let removed = last_content.is_some_and(|index| !commits[index].new_buffer);
        let buffer_attached = last_content.is_some_and(|index| commits[index].new_buffer);
        let mut callbacks = Vec::new();
        let mut superseded = 0;
        for (index, mut commit) in commits.into_iter().enumerate() {
            let taken = mem::take(&mut commit.callbacks);
            if removed || last_content.is_some_and(|last| index < last) {
                superseded += taken.len();
                for callback in taken {
                    callback.discarded();
                }
            } else {
                callbacks.extend(taken);
            }
        }
        // Unstaged feedback rides along with the kept commits: a surface
        // created before the hook, and a synchronised child whose pending
        // state its parent's commit pushed into the cache without running
        // the child's pre-commit hooks (vendored smithay
        // `commit_sync_surface_tree`, tree.rs). The latter is the same
        // Smithay-only handling as before staging existed.
        callbacks.extend(unstaged);
        let record = self
            .surfaces
            .get(&surface.id())
            .filter(|record| !matches!(record.role, SurfaceRole::Dormant(_)));
        if superseded > 0 {
            let (id, seq) = record.map_or((0, 0), |record| (record.id.0, record.content_seq));
            crate::frame_trace::event("comp_presentation_discarded", || {
                (id, seq, DiscardReason::Superseded as u64)
            });
        }
        if callbacks.is_empty() {
            return;
        }
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
        match self.presentation.refused.get(&id) {
            Some(refused) if *refused == seq => {
                // A bufferless commit on top of refused content.
                for callback in callbacks {
                    callback.discarded();
                }
                crate::frame_trace::event("comp_presentation_discarded", || {
                    (id.0, seq, DiscardReason::Refused as u64)
                });
                return;
            }
            Some(refused) if *refused < seq => {
                self.presentation.refused.remove(&id);
            }
            _ => {}
        }
        let overflow = self.presentation.ledger.take_on_commit(id, seq, callbacks);
        trace_discards(id, &overflow, DiscardReason::Overflow);
    }

    /// Pre-commit hook: move this commit's feedback out of Smithay's pending
    /// state, tagged with whether the commit replaced the buffer.
    pub(super) fn stage_presentation_feedback(&self, surface: &WlSurface) {
        compositor::with_states(surface, |states| {
            let callbacks = mem::take(
                &mut states
                    .cached_state
                    .get::<PresentationFeedbackCachedState>()
                    .pending()
                    .callbacks,
            );
            let (supersedes, new_buffer) = {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let buffer = &attributes.pending().buffer;
                (
                    buffer.is_some(),
                    matches!(buffer, Some(BufferAssignment::NewBuffer(_))),
                )
            };
            if callbacks.is_empty() && !supersedes {
                return;
            }
            states
                .cached_state
                .get::<StagedFeedback>()
                .pending()
                .commits
                .push(StagedCommit {
                    callbacks,
                    supersedes,
                    new_buffer,
                });
        });
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
        self.presentation.refused.remove(&id);
        self.presentation.stats.forget_surface(id.0);
    }

    /// `(id, generation)` of the window `surface` belongs to: itself for a
    /// toplevel, its root for a subsurface, none for popups, layers, locks.
    fn stats_window(&self, surface: &WlSurface) -> Option<(u64, u64)> {
        let root = self.toplevel_root_for_surface(surface)?;
        let record = self.surfaces.get(&root.id())?;
        Some((record.id.0, record.generation))
    }

    /// A new buffer became `surface`'s content (its `content_seq` was just
    /// advanced): start timing it.
    pub(super) fn note_content_published(&mut self, surface: &WlSurface) {
        let Some(record) = self.surfaces.get(&surface.id()) else {
            return;
        };
        let (id, seq) = (record.id.0, record.content_seq);
        let window = self.stats_window(surface);
        self.presentation
            .stats
            .note_published(id, window, seq, crate::frame_trace::monotonic_us());
    }

    /// THE hook for injected input (the `comp.input.*` verbs call it once
    /// per injection, with their own `input_seq` counter): `target` is the
    /// surface the input was delivered to, or none. Its window (the root
    /// toplevel, through subsurfaces and popups) records `input_to_present`
    /// for the first update committed afterwards, within a second; a content
    /// source's update that names `input_seq` does the same. `input_seq`
    /// must increase across all injections: an old or repeated one is
    /// refused (returns false).
    pub(crate) fn note_injected_input(
        &mut self,
        target: Option<SurfaceId>,
        mark: InputMark,
    ) -> bool {
        let window = target
            .and_then(|id| self.surface_objects.get(&id))
            .and_then(|object| self.surfaces.get(object))
            .map(|record| canonical_root_surface(&self.popup_manager, record.role.wl_surface()))
            .and_then(|root| self.stats_window(&root));
        let accepted = self.presentation.stats.mark_input(window, mark);
        if !accepted {
            tracing::warn!(
                input_seq = mark.input_seq,
                "injected input mark refused: input_seq is not newer than the last one"
            );
        }
        accepted
    }

    pub(super) fn commit_refused(&mut self, id: SurfaceId, sampled: Option<u64>, seq: u64) {
        let from = sampled.map_or(seq, |sampled| sampled.saturating_add(1));
        let resolution = self.presentation.ledger.discard_range(id, from, seq);
        trace_discards(id, &resolution, DiscardReason::Refused);
        let refused = self.presentation.refused.entry(id).or_default();
        *refused = (*refused).max(seq);
    }

    /// A KMS flip is presented on the client output registered for its key.
    /// A key without one (not yet, or no longer, a client output) cannot
    /// name a presentation, so the report is dropped and feedback waits.
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(super) fn kms_frame_presented(
        &mut self,
        key: &crate::backend::kms::OutputKey,
        frame: PresentedFrame,
        content: FrameContent,
    ) {
        let Some(output) = self
            .backend
            .kms_registered_outputs()
            .into_iter()
            .find_map(|(registered, output)| (registered == *key).then_some(output))
        else {
            tracing::debug!(
                connector = key.connector_name,
                "KMS flip on an output with no client output; no surface is presented on it"
            );
            // The flip happened and the renderer already handed over this
            // frame's content-source costs, so account for them; only
            // surface feedback needs an output to name.
            self.content_sources_presented(&frame, &content);
            return;
        };
        self.frame_presented(
            PresentedFrame {
                output: Some(output),
                ..frame
            },
            content,
        );
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
        let refresh_us = match frame.refresh {
            // A variable rate has no vblank grid to count misses against.
            Refresh::Fixed(refresh) => u64::try_from(refresh.as_micros()).ok(),
            Refresh::Unknown | Refresh::Variable(_) => None,
        };
        let mut reported = HashSet::new();
        let mut windows = HashMap::<u64, (u64, WindowFrame)>::new();
        // Once per report, not once per surface (see `workspaces::on_workspace`).
        let current_workspace = self.workspace_current();
        for surface in &content.surfaces {
            reported.insert(surface.id);
            let record = self
                .surface_objects
                .get(&surface.id)
                .and_then(|object| self.surfaces.get(object));
            // Off the current workspace is Hidden, never Waiting: the
            // renderer will not sample it until a switch brings it back.
            let presentable = record.is_some_and(|record| {
                record.mapped
                    && !record.minimized
                    && super::workspaces::on_workspace(record, current_workspace)
                    && (!lock_active || self.surface_is_session_presentable(record))
            });
            let shown = surface.shown && presentable;
            let state = if shown {
                SurfaceShown::Shown
            } else if surface.waiting && presentable {
                SurfaceShown::Waiting
            } else {
                SurfaceShown::Hidden
            };
            if let Some((window, generation)) = record
                .map(|record| record.role.wl_surface().clone())
                .and_then(|wl_surface| self.stats_window(&wl_surface))
            {
                let (_, fold) = windows
                    .entry(window)
                    .or_insert_with(|| (generation, WindowFrame::default()));
                self.presentation.stats.surface_frame(
                    surface.id.0,
                    window == surface.id.0,
                    surface.commit_seq,
                    state,
                    fold,
                );
            }
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
        self.presentation
            .stats
            .hide_unlisted(|window| windows.contains_key(&window));
        for (window, (generation, fold)) in windows {
            self.presentation
                .stats
                .window_frame(window, generation, fold, time_us, refresh_us);
        }
        if let Some(output) = &frame.output {
            self.presentation.stats.output_frame(
                &output.name(),
                time_us,
                frame.flags.bits(),
                refresh_us,
            );
        }
        self.content_sources_presented(&frame, &content);
    }

    /// Fold this frame's in-process content sources into their ledger. They
    /// name no output, so this is the same work whether or not the frame
    /// could be presented to clients.
    fn content_sources_presented(&mut self, frame: &PresentedFrame, content: &FrameContent) {
        if content.sources.is_empty() {
            return;
        }
        let time_us = u64::try_from(frame.time.as_micros()).unwrap_or(u64::MAX);
        let refresh_us = match frame.refresh {
            Refresh::Fixed(refresh) => u64::try_from(refresh.as_micros()).ok(),
            Refresh::Unknown | Refresh::Variable(_) => None,
        };
        let PresentationRuntime { sources, stats, .. } = &mut self.presentation;
        for source in &content.sources {
            sources.resolve(source, time_us, refresh_us, |input_seq, at_us| {
                stats.input_mark(input_seq, at_us)
            });
        }
    }

    pub(super) fn content_source_registered(&mut self, id: &str, output: Option<String>) {
        let registration =
            self.presentation
                .sources
                .register(id, output, crate::frame_trace::monotonic_us());
        tracing::debug!(source = id, registration, "content source registered");
    }

    pub(super) fn content_source_unregistered(&mut self, id: &str, revision: u64) {
        if let Some(counters) = self.presentation.sources.unregister(id, revision) {
            tracing::debug!(
                source = id,
                presented = counters.stats.presented,
                discarded = counters.stats.discarded,
                "content source unregistered"
            );
        }
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod ledger_tests;
