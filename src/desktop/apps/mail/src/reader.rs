//! The reading pane, and the body swap that connects it to the list.
//!
//! ## Why the swap despawns and respawns
//!
//! `CtkBodyViewProps::new` takes the `SanitizedBody` at construction and CTK
//! exposes no way to replace a live view's content — `set_body_render_arm` is
//! the only mutator. So selecting a different message tears the view down and
//! builds a new one.
//!
//! That is a real gap, and this slice was supposed to decide whether it is a
//! gap worth an API. It measured the swap rather than assuming
//! ([`ReaderStats`], reported by `probe`), because "respawn is obviously too
//! slow" is exactly the kind of claim that turns out to be false once the cost
//! is dominated by work the respawn is not responsible for.
//!
//! Note what that does *not* establish about a setter. A setter is only bound to
//! repeat the dominant work if it must redo the same materialisation; one taking
//! already-sanitised content, recognising an unchanged `Arc`, or keeping the
//! block entities and rebinding them need not. Reading the attribution below as
//! "so a setter would save nothing" smuggles in an implementation of the API
//! nobody has written.
//!
//! **Verdict: `set_body` unmeasured, and not requested.** Not the same thing as
//! "no `set_body`", which is what this line said for three drafts while the
//! analysis below said the opposite — every run here goes through
//! despawn-and-respawn, so nothing measured what an in-place setter would cost.
//! What the runs do settle is that the ~400 ms is caused by *materialising* a
//! long body through this path, and that the fix worth asking CTK for is block
//! virtualisation inside `body_view`, not a mutator on top of the same
//! materialisation. Measured on this workstation, release build,
//! 50 000-row corpus (`--probe`, 2026-07-31). Means, not quantiles: every
//! `LatencyHistogram` mean is exact — it divides a duration sum — while the
//! *quantiles* are bucketed with an open-ended top bucket, so one landing there
//! degenerates to the observed max (`ctk/src/latency.rs:81`). The swap column is
//! a lower bound on frame impact because work downstream of it sits outside its
//! timer, not because the mean is coarse.
//!
//! | run | body, and what happens to it | timed swap | mean frame |
//! |---|---|---|---|
//! | `200:50000:0` | newsletter, held after one swap | 0.6 ms | 16.8 ms |
//! | `200:5:0` | newsletter, swapped every frame | 0.8 ms | 16.9 ms |
//! | `200:50000:4` | 400-para digest, held after one swap | 8.4 ms | 18.7 ms |
//! | `200:5:4` | 400-para digest, swapped every frame | 14.0 ms | 413.3 ms |
//!
//! 16.8 ms is the 60 Hz vsync floor. These are **separate** runs, so each
//! frame mean carries its own unmeasured floor — `list.rs` explains why that
//! matters there and pairs its two strides inside one process to escape it.
//! It is not worth doing here: the effect this table is claiming is a ~400 ms
//! difference against a ~17 ms floor, two orders of magnitude past what floor
//! drift can manufacture. The one caveat that does apply is load — every run
//! above was taken on an idle machine, and the same binary under load average
//! 16–17 has come back between two-thirds and thirty times dearer, which is
//! not a factor to correct for but a run to discard.
//!
//! Three things fall out, and none of them were guessable from the swap
//! timing alone:
//!
//! 1. **Swapping a short body is free** — 16.9 vs 16.8 ms is noise.
//! 2. **Swapping a long one costs ~400 ms of frame, of which the timed swap is
//!    14 ms.** The swap function — despawn, sanitise, project, spawn, flush —
//!    accounts for about 3% of what showing that message actually costs. The
//!    other ~97% is *downstream of materialising 400 paragraphs*: the
//!    differencing shows what causes it, not what it consists of. Shaping and
//!    UI layout are the obvious candidates and no run here separates them from
//!    render extraction or presentation pacing, and the argument does not need
//!    them separated — whatever that work is, it is caused by *materialising*
//!    400 paragraphs through this path.
//! 3. **The cost is entirely one-time.** The held-digest run means 18.7 ms with
//!    a single 428 ms frame in it; the remaining 199 frames average 16.6 ms —
//!    the floor. Holding a 400-paragraph body on screen costs nothing per
//!    frame, so nothing is being re-laid-out.
//!
//! What that does **not** establish is the value of an in-place `set_body`, and
//! an earlier version of this paragraph claimed it did — that such a setter
//! "would pay the ~400 ms in full" and save only the churn inside the 14 ms
//! timer. No run here measures an in-place path, because there is no in-place
//! path to run: every measurement above goes through despawn-and-respawn. A
//! setter that reused block entities, kept their layout, or no-opped on
//! unchanged content might pay much less, and the probe's own repeats make the
//! point — it re-selects messages sharing one `Arc<BodySource>` (`store.rs`),
//! so what is measured is the same body rebuilt from scratch each time. The
//! honest statement is that *this* path is expensive; whether a different one
//! would be is unmeasured, and measuring it needs the in-place control that
//! does not exist yet.
//!
//! The mixed run cross-checks the arithmetic: `600:37` cycles
//! five fixtures, so one frame in five materialises the digest, and its mean
//! frame is 104.1 ms ≈ 16.8 floor + ~6 rebind + 0.2 × ~400.
//!
//! **The gap that is real is a different one:** `body_view` materialises every
//! block, so selecting a long message stalls the UI for ~400 ms — 24 dropped
//! frames, well past the point where a click stops feeling connected. That
//! wants block-level virtualisation or incremental layout inside `body_view` —
//! the same argument `virtual_list` already won. Recorded as a finding here,
//! not fixed: it is a CTK change with its own design and review.
//!
//! The existing `BODY_VIEW_MAX_BLOCKS` (4 096) does not save this and is not
//! meant to: it is a pathology guard against DOM-to-entity expansion, not
//! viewport virtualisation. 400 paragraphs is a tenth of that budget and
//! already costs 400 ms, so the budget bites roughly an order of magnitude past
//! the point where the widget stops being usable.
//!
//! ## Remote images: what this pane can and cannot do
//!
//! The sanitiser strips remote references and inventories them, and the app
//! can show that inventory. It cannot *load* them, and this is a hard limit,
//! not an unfinished feature: `ProjectedBlockKind` is
//! Paragraph/Heading/Preformatted/Rule/Truncated with no image variant, so the
//! text arm has nothing to draw an image with, and neither `BodySource::sanitize`
//! nor `sanitize_html` takes a policy argument, so there is no allow-remote mode
//! to re-admit the URLs even if it could. The "Remote content" action therefore
//! reveals counts and URLs and says plainly that nothing will be fetched.
//!
//! Note which half is actually missing: the sanitiser *keeps* embedded `data:`
//! images (`sanitizer_removes_remote_pixels_but_keeps_embedded_images`), and
//! those still cannot be displayed. So the blocker is the projection and render
//! side, not only the fetch policy — a real image path needs an image block
//! kind first, and a policy switch second. A CTK change this slice does not make.

use std::sync::Arc;
use std::time::Instant;

use bevy::prelude::*;
use ctk::latency::LatencyHistogram;
use ctk::prelude::{spawn_body_view, BodySource, CtkBodyView, CtkBodyViewProps};

use crate::store::{MailRowId, MessageStore};

/// Application handle on the store, shared with the list model.
#[derive(Resource, Clone)]
pub struct Corpus(pub Arc<dyn MessageStore>);

/// Reading-pane state.
#[derive(Resource)]
pub struct Reader {
    /// Slot the body view is parented into.
    pub host: Entity,
    /// Message currently shown, if any.
    pub current: Option<MailRowId>,
    /// Root of the live body view, despawned on the next swap.
    view: Option<Entity>,
    pub stats: ReaderStats,
    /// Inventory of the shown message, for the remote-content action.
    pub remote: RemoteSummary,
}

impl Reader {
    pub fn new(host: Entity) -> Self {
        Self {
            host,
            current: None,
            view: None,
            stats: ReaderStats::default(),
            remote: RemoteSummary::default(),
        }
    }

    /// Whether `entity` is the body view currently on screen.
    ///
    /// Body-view events name their origin view and arrive deferred, so an
    /// event from a view this reader has already despawned must not be acted
    /// on. `None` — nothing shown — is never current.
    pub fn is_current_view(&self, entity: Entity) -> bool {
        self.view == Some(entity)
    }
}

/// Cost of body swaps, measured end to end.
#[derive(Default)]
pub struct ReaderStats {
    /// Whole swap: despawn, sanitise, project, spawn, flush.
    pub swap: LatencyHistogram,
    /// The sanitise step alone, to attribute the cost honestly.
    ///
    /// What it answers is where the time goes, not what an API would save: a
    /// setter that re-sanitised would save nothing here, one handed cached
    /// sanitised content would skip this column entirely. The measurement
    /// separates the steps; it does not choose the setter's contract.
    pub sanitize: LatencyHistogram,
    pub swaps: u64,
}

/// What the sanitiser found and suppressed in the shown body.
#[derive(Clone)]
pub struct RemoteSummary {
    pub count: usize,
    /// False when CTK could not prove the inventory exhaustive — the count is
    /// then a floor.
    ///
    /// Deliberately not "hit the cap": `collect_remote_refs` clears this for
    /// several unrelated reasons, most of which involve no truncation at all —
    /// an `<iframe srcdoc>` whose nested document it will not parse
    /// (`ctk/src/body_view.rs:694`), any node outside the XHTML namespace
    /// (`:711`), an unparseable or too-deep CSS `url()`. Reading it as
    /// "truncated" and saying so to the user is a specific claim about a
    /// general flag, which is how the first version of [`describe_remote`] came
    /// to tell people their mail had been cut short when it had not.
    ///
    /// It is nonetheless the **single** authority on whether the inventory can
    /// be trusted, truncation included: `sanitize_html` ends with
    /// `remote_refs.complete &= !input_truncated`
    /// (`ctk/src/body_view.rs:443`), so a body cut short can never come back
    /// claiming a complete inventory. Anding [`Self::input_truncated`] into the
    /// test again is therefore redundant for HTML and wrong for plain text,
    /// which returns `RemoteRefs::default()` — complete, because a plain-text
    /// body has no references to miss (`:369`) — whatever its length.
    pub complete: bool,
    pub sample: Vec<String>,
    /// Whether CTK stopped reading the source body at its input cap.
    ///
    /// A fact about the *body*, not about the inventory: see [`Self::complete`]
    /// for why it must not be folded into the trust test.
    pub input_truncated: bool,
}

/// `Default` is the reader's state before anything is shown, and `complete`
/// must be `true` there.
///
/// Not a derive, because `bool::default()` is `false` and that is the *unsafe*
/// value for this field: `describe_remote` reads `complete == false` as "I
/// stopped looking before I finished", so a derived default would have a
/// freshly-opened window claim its empty pane had been truncated mid-inspection.
/// An empty body genuinely was inspected in full and genuinely has nothing.
impl Default for RemoteSummary {
    fn default() -> Self {
        Self {
            count: 0,
            complete: true,
            sample: Vec::new(),
            input_truncated: false,
        }
    }
}

/// Replace the reading pane's contents with `id`.
///
/// Takes the body rather than fetching it. The reader used to read the store
/// itself, which made the pane a second, independent reader of a mutable
/// corpus: the caller read the summary, this read the body, and between the two
/// acquisitions the store was free to change — so a heading could end up over a
/// body that was never the message it names. The caller now reads both from one
/// snapshot (`MessageStore::message`) and hands the body down, which is why the
/// "message vanished between selection and swap" arm that used to live here is
/// gone: there is no longer a second read to lose the race.
///
/// Returns the id the pane shows **after** this call: `id` whenever the swap
/// happened, `None` when there is no reader to swap in (the resource is
/// missing, which means the shell has not finished starting or is tearing
/// down). Callers need the id rather than a bare success flag
/// because everything else in the header has to describe whatever body is
/// actually on screen; a swap that silently declined while its caller went on to
/// relabel the pane is precisely how a heading ends up over the wrong message.
/// That is now the only way to decline — the store-miss arm moved out with the
/// store read.
///
/// Runs with exclusive world access so the measurement includes the command
/// flush. Measuring only the `Commands` queueing would report a fraction of
/// the real cost and flatter the current design.
pub fn swap_body(world: &mut World, id: MailRowId, source: BodySource) -> Option<MailRowId> {
    let reader = world.get_resource::<Reader>()?;
    // No "already showing this id, nothing to do" short-circuit, deliberately.
    // It was here, and it was the same wrong-message bug wearing its last
    // disguise: an id is an *identity*, not a content revision. A store that
    // edits a message in place — a JMAP push rewriting a body, a draft being
    // saved — keeps the id and changes everything the pane draws, and the fast
    // path would then keep the old body while the caller, holding a fresh
    // snapshot, wrote the new heading above it. Re-rendering an unchanged
    // message costs one body view respawn on a path a human reaches by
    // re-selecting the row they are already on; believing an id is a revision
    // costs a wrong body under a right heading, with no way to notice.
    let host = reader.host;
    let previous = reader.view;

    let started = Instant::now();
    if let Some(previous) = previous {
        if let Ok(entity) = world.get_entity_mut(previous) {
            entity.despawn();
        }
    }

    let sanitize_started = Instant::now();
    let body = source.sanitize();
    let sanitize_elapsed = sanitize_started.elapsed();

    let remote = RemoteSummary {
        count: body.remote_refs().count(),
        complete: body.remote_refs().is_complete(),
        sample: body.remote_refs().urls().iter().take(4).cloned().collect(),
        input_truncated: body.input_truncated(),
    };

    let entities = {
        let mut commands = world.commands();
        let view = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Message body").viewport_height(BODY_VIEWPORT),
        );
        commands.entity(host).add_child(view.root);
        view
    };
    world.flush();
    let elapsed = started.elapsed();

    let Some(mut reader) = world.get_resource_mut::<Reader>() else {
        // The view is spawned and parented; the pane shows `id` whether or not
        // the bookkeeping resource survived to record it.
        return Some(id);
    };
    reader.current = Some(id);
    reader.view = Some(entities.root);
    reader.remote = remote;
    reader.stats.swap.record(elapsed);
    reader.stats.sanitize.record(sanitize_elapsed);
    reader.stats.swaps += 1;

    // Marking read after the body is up keeps the read receipt honest: the
    // message really was rendered. `mark_read` is `false` when the row was
    // already read, so an unchanged row never costs the list a rebind.
    let marked = world
        .get_resource::<Corpus>()
        .cloned()
        .is_some_and(|corpus| corpus.0.mark_read(id));
    if marked {
        if let Some(mut pending) = world.get_resource_mut::<PendingRebinds>() {
            // The **id**, not the index it currently sits at. This queue is
            // drained at the end of the frame, and a position stored across
            // that gap is a position an insertion can reassign — the rebind
            // would then refresh whichever message had moved into the slot
            // while the one just read stayed visibly unread.
            pending.0.push(id);
        }
    }
    Some(id)
}

/// Height handed to every body view. The reading pane is a fixed slot in the
/// shell centre, so this is a constant rather than a measured layout value.
const BODY_VIEWPORT: f32 = 520.0;

/// Messages whose summary changed and need rebinding, drained by the app so the
/// list is told once per frame instead of once per message.
///
/// Identities, not positions. CTK's change hint is an index range and this queue
/// therefore has to become one — but the conversion happens in `drain_rebinds`,
/// at the moment the hint is issued, so nothing here survives a mutation of the
/// store. The residual is CTK's: even a freshly resolved index is a position
/// handed to a reconcile that runs later in the schedule. Closing that would
/// take an id-keyed change hint in `virtual_list`, which is a widget API change
/// this slice has not earned — the fixture corpus is immutable and cannot
/// exercise the gap. It goes in the ledger for the JMAP store, which can.
#[derive(Resource, Default)]
pub struct PendingRebinds(pub Vec<MailRowId>);

/// Read the live view's own inventory, which is the authority once spawned.
///
/// [`Reader::remote`] is captured pre-spawn from the same `SanitizedBody`, so
/// the two agree; this exists so the action reports what the *widget* holds
/// rather than what the app remembered.
pub fn live_remote_summary(world: &World) -> Option<RemoteSummary> {
    let reader = world.get_resource::<Reader>()?;
    let view = reader.view?;
    let body_view = world.get::<CtkBodyView>(view)?;
    let refs = body_view.remote_refs();
    Some(RemoteSummary {
        count: refs.count(),
        complete: refs.is_complete(),
        sample: refs.urls().iter().take(4).cloned().collect(),
        input_truncated: reader.remote.input_truncated,
    })
}

/// Human-readable line for the remote-content action.
pub fn describe_remote(summary: &RemoteSummary) -> String {
    // "None found" and "none found before I stopped looking" are different
    // statements, and only one of them is safe to show next to a privacy
    // control. A body whose inspection stopped early has `count == 0` with an
    // incomplete inventory; saying "no remote content" there tells the user the
    // message is clean when nothing established that.
    //
    // The trust test is `complete` **alone**. An earlier version also required
    // `!input_truncated`, on the reasoning that a body read only in part cannot
    // have been inventoried in full — true, and already handled one layer down:
    // `sanitize_html` clears `complete` on truncation itself
    // (`ctk/src/body_view.rs:443`). The extra term bought nothing for HTML and
    // was actively wrong for plain text, where truncation is possible but
    // remote references are not, so it labelled a long plain-text message
    // "unknown, not clean" over an inventory that was never in doubt.
    //
    // Truncation is still worth saying — it is a fact about the body the reader
    // is looking at — so it is said as its own sentence rather than smuggled
    // into the privacy verdict.
    let mut line = if summary.count == 0 {
        if summary.complete {
            "No remote content in this message.".to_string()
        } else {
            "No remote content found, but parts of it could not be inspected — \
             treat that as unknown, not clean."
                .to_string()
        }
    } else {
        let qualifier = if summary.complete { "" } else { " (at least)" };
        let sample = summary
            .sample
            .iter()
            .map(|url| elide_url(url, 58))
            .collect::<Vec<_>>()
            .join("  ·  ");
        format!(
            "{}{qualifier} remote reference{} blocked — not fetched, and this build cannot display them.  {sample}",
            summary.count,
            if summary.count == 1 { "" } else { "s" },
        )
    };
    if summary.input_truncated {
        // Two different facts, and which one is true depends on `complete`.
        // Truncation always means the *body* is partial, so it is always worth
        // saying. Whether it also limits the *inventory* is a separate question
        // that `complete` has already answered: an HTML body that was truncated
        // has `complete == false` (`ctk/src/body_view.rs:443`), so the caveat
        // belongs there — but a plain-text body is truncated with `complete ==
        // true`, because plain text has no construct that could fetch anything,
        // and "this covers the part that was read" would be inviting the user to
        // doubt a report that is exhaustive. Attaching the caveat to truncation
        // rather than to incompleteness is what made it wrong.
        line.push_str(if summary.complete {
            "  The message was too large to show in full."
        } else {
            "  The message was too large to show in full, so this covers the part that was read."
        });
    }
    line
}

fn elide_url(url: &str, limit: usize) -> String {
    // Character-based: a byte slice would split a multi-byte host name.
    let count = url.chars().count();
    if count <= limit {
        return url.to_string();
    }
    let head: String = url.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_remote_says_plainly_that_nothing_is_fetched() {
        let summary = RemoteSummary {
            count: 3,
            complete: true,
            sample: vec!["https://cdn.example.com/logo.png".into()],
            input_truncated: false,
        };
        let line = describe_remote(&summary);
        assert!(line.contains("3 remote references blocked"));
        assert!(line.contains("not fetched"));
    }

    #[test]
    fn an_incomplete_inventory_is_reported_as_a_floor() {
        let summary = RemoteSummary {
            count: 64,
            complete: false,
            sample: Vec::new(),
            input_truncated: false,
        };
        assert!(describe_remote(&summary).contains("(at least)"));
    }

    #[test]
    fn no_remote_content_is_its_own_sentence() {
        let line = describe_remote(&RemoteSummary::default());
        assert_eq!(line, "No remote content in this message.");
    }

    #[test]
    fn a_zero_count_over_an_unfinished_inspection_is_not_reported_as_clean() {
        // The dangerous case: nothing found *because the search stopped*. Saying
        // "no remote content" there is a privacy claim nothing established.
        let line = describe_remote(&RemoteSummary {
            complete: false,
            ..Default::default()
        });
        assert!(
            line.contains("unknown, not clean"),
            "an unfinished inspection must not read as a clean bill of health.  \
             Got: {line}"
        );
    }

    #[test]
    fn truncation_alone_does_not_make_the_inventory_untrusted() {
        // `complete && input_truncated` is reachable only from a plain-text
        // body: `sanitize_html` clears `complete` when it truncates
        // (`ctk/src/body_view.rs:443`), while `BodySource::Plain` returns a
        // complete `RemoteRefs` because plain text carries no references to
        // miss (`:369`). So this combination is a long plain message, and it
        // genuinely is clean of remote content — calling it "unknown" would be
        // a warning about nothing, which is how privacy controls get ignored.
        let line = describe_remote(&RemoteSummary {
            input_truncated: true,
            ..Default::default()
        });
        assert!(line.starts_with("No remote content in this message."));
        assert!(!line.contains("unknown, not clean"));
        // The truncation is still reported — as a fact about the body.
        assert!(line.contains("too large to show in full"));
        // …and only about the body. Saying the report "covers the part that was
        // read" here would undercut an inventory that is exhaustive: plain text
        // has nothing that could fetch, truncated or not.
        assert!(!line.contains("covers the part that was read"));
    }

    #[test]
    fn an_incomplete_inventory_does_not_claim_the_message_was_truncated() {
        // `complete` is cleared by a nested `srcdoc`, foreign markup or
        // unparseable CSS, none of which shorten the body. Reporting those as
        // "too large" is a specific false claim, not a cautious one.
        let line = describe_remote(&RemoteSummary {
            complete: false,
            ..Default::default()
        });
        assert!(
            line.contains("parts of it could not be inspected"),
            "an incomplete inventory must name the right reason.  Got: {line}"
        );
        assert!(!line.contains("too large"));
    }

    #[test]
    fn a_truncated_body_with_references_says_the_count_covers_only_what_was_read() {
        let line = describe_remote(&RemoteSummary {
            count: 2,
            complete: false,
            sample: vec!["https://cdn.example.com/a.png".into()],
            input_truncated: true,
        });
        assert!(line.contains("(at least)"));
        assert!(line.contains("too large to show in full"));
        // Here the caveat *is* earned: an HTML body that was truncated has an
        // inventory limited by the truncation, which is why `complete` is false.
        assert!(line.contains("covers the part that was read"));
    }

    #[test]
    fn elide_url_never_splits_a_multibyte_character() {
        let url = format!("https://例え.example.org/{}", "パス".repeat(40));
        let elided = elide_url(&url, 20);
        assert_eq!(elided.chars().count(), 20);
        assert!(elided.ends_with('…'));
    }

    #[test]
    fn a_short_url_is_left_alone() {
        assert_eq!(
            elide_url("https://example.org/a", 58),
            "https://example.org/a"
        );
    }
}
