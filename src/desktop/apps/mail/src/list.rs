//! The message list: a [`VirtualListModel`] over [`MessageStore`].
//!
//! ## The uniform-height question, answered empirically
//!
//! CTK's virtual list v1 gives every row one fixed height. The open question
//! this slice was built to settle was whether a *real* mail summary row
//! survives that constraint, or whether the widget needs a variable-height
//! extension point before a mail client can use it.
//!
//! It survives, and the reason is not "we made the row simpler". A summary row
//! carries a fixed set of fields — sender, date, subject, snippet — every one
//! of which is single-line **by choice, not by luck**: a wrapped subject in a
//! list row is a readability regression, not a feature, which is why every
//! mail client clips rather than reflows it. The row is three text lines whose
//! count does not depend on content, so its natural height is constant and
//! [`ROW_HEIGHT`] is a statement of that constant rather than a cap fighting
//! the content.
//!
//! What would break the constraint is a genuinely variable row: inline
//! attachment chips that wrap, or a thread row that grows a child line per
//! unread reply. Neither is in this slice, and neither is speculative enough
//! to open a widget extension point for today. **No `virtual_list` API change
//! is requested.** If threading lands and forces it, that is when the
//! variable-height ADR gets written — with a real consumer in hand, which was
//! the whole point of building the slice before the extension point.
//!
//! ## Recycling under rapid selection, measured
//!
//! The other thing the slice was meant to find out — and it had to be measured
//! twice, because the first answer came from the wrong instrument.
//!
//! `VirtualList::latency()` times `reconcile_virtual_lists` through
//! `paint_virtual_rows` (`ctk/src/virtual_list.rs:669`, `:1243`), both in
//! `Update`. Bevy shapes text and lays out UI in `PostUpdate`, so a rebind's
//! downstream cost is outside that histogram by construction. It reads p99 ≤
//! 2.0 ms at one full rebind per frame and that number is real but partial.
//!
//! The honest figure is a **paired** difference — both strides alternated
//! inside one process in 10-frame blocks, in ABBA order so half the pairs run
//! each arm first, so each rebind block is compared against its immediate
//! neighbour rather than against a separate run's unmeasured frame floor
//! (`--probe 600:5:0:25`; release, 50 000 rows, 24-row realised window,
//! 2026-07-31, three runs on an idle machine, 28 counterbalanced pairs each —
//! 29 are measured and the odd one is trimmed so exactly half the kept pairs run
//! each arm first):
//!
//! | run | stride 5 (shift) | stride 25 (rebind) | mean difference | median pair | signs |
//! |---|---|---|---|---|---|
//! | 1 | 15.9 ms | 22.4 ms | +6.5 ms | +6.2 ms | 28–0–0 |
//! | 2 | 16.0 ms | 21.2 ms | +5.2 ms | +5.1 ms | 28–0–0 |
//! | 3 | 15.9 ms | 21.6 ms | +5.7 ms | +5.7 ms | 28–0–0 |
//!
//! **A full rebind of the window costs a few milliseconds of frame** — about
//! 3× what CTK's own histogram reports, which is what you would expect once
//! shaping 24 rows of text is counted.
//!
//! What that table does and does not establish, because the difference between
//! the two is where this measurement kept going wrong:
//!
//! - **The direction is settled, and it is *replication* that settles it.**
//!   Every pair of every run fell the same way; the same binary run with its
//!   arms *reversed* (`--probe 600:25:0:5`) returns −6.2 ms, median −6.3, 0–28–0
//!   — the same magnitude with the sign flipped; and three separate runs whose
//!   arms inflated to 112/205.8 ms, 582.6/616.7 ms and 1269.9/23.1 ms still
//!   reported medians of +6.6, +6.4 and +6.5 ms. The test is two-sided, so no
//!   result here is the assumption that produced it.
//!
//!   The per-run `p` is deliberately **not** the argument, and this is the one
//!   place the report's own number should be read down rather than up. An exact
//!   sign test assumes the pairs are independent draws; they are adjacent blocks
//!   on one presentation timeline, where a scheduling or thermal regime lasting
//!   several blocks correlates their signs, so 28 agreeing pairs can be one
//!   run-level state rather than 28 coin flips. It is an exact probability under
//!   an assumption that does not hold, and not a bound in either direction —
//!   dependence can make it too small or too large. What settles the direction
//!   is seven runs agreeing,
//!   including one with the arms swapped — a between-run fact no within-run
//!   statistic can supply.
//! - **The magnitude is a range, not a bracket.** Separate runs say 4.5–6.1 ms
//!   and the paired design says 5.2–6.5 ms; the truth is not proven to lie
//!   between them, because neither method's bias direction is established. The
//!   paired shift arm still reads ~0.9 ms below its own unpaired value
//!   (16.8 ms), which is residual boundary displacement — down from 1.4 ms
//!   before the ABBA counterbalance, so the design is doing what it claims,
//!   just not perfectly. Everything below rests only on "a few milliseconds".
//!
//! A few milliseconds is still not a mail client's problem: a full rebind only
//! happens when the selection jumps further than the window in a single frame,
//! which a human cannot do with an arrow key. See `reader.rs` for where the
//! time actually goes — the same experiment puts one body materialisation at
//! ~400 ms.
//!
//! Two things this measurement had to survive, both recorded because they will
//! bite the next person to touch the probe:
//!
//! - **Frame-by-frame alternation answers backwards.** Under vsync an
//!   overrunning frame presents late and the next frame finds most of an
//!   interval already gone, so each arm hands its overrun to the other. The
//!   first paired build alternated per frame and reported the rebind stride
//!   *faster*, 86 of 90 pairs inverted. Blocks of 10 frames with 2 settle
//!   frames dropped at each boundary fixed it.
//! - **Machine load inflates both arms, and not by a fixed amount.** Runs taken
//!   while another build had the machine read 25.5/37.2, 35.5/49.1, 112.0/205.8
//!   and 582.6/616.7 ms against the table above — between a two-thirds and a
//!   thirty-fold inflation, so there is no correction factor to apply, only a
//!   run to discard. Check `uptime` before believing a probe number.
//!
//!   The per-pair *median* is the number that survives it, and one run makes the
//!   case better than the rest: at load 15.3 the arms read 1269.9/23.1 ms, so
//!   the **difference of means was −1246.8 ms** — opposite in sign to the
//!   replicated direction, by two orders of magnitude, on a run whose median was
//!   a perfectly ordinary +6.5 ms with 24 of 28 pairs still dearer.
//!
//!   Both numbers are arithmetically right, and they are not the same estimand:
//!   the mean is average observed cost, which a handful of enormous stalls
//!   genuinely dominate, and the median is the typical pair. Calling the mean
//!   "wrong" would assume those stalls were unrelated to the treatment, which
//!   this run cannot establish — under load it is not knowable whether they fell
//!   on one arm by chance. What the run does establish is that a mean of block
//!   means is one stalled block away from pointing against seven replications,
//!   and that the two statistics disagreeing is the signal to retake. That is
//!   the whole argument for printing both.

use bevy::a11y::AccessibilityNode;
use bevy::feathers::theme::{ThemeTextColor, ThemeToken};
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::text::FontWeight;
use ctk::prelude::{RowId, VirtualListModel, VirtualListRow};
use ctk::theme::tokens;

use crate::store::{MailRowId, MessageStore, PLACEHOLDER_ID_BIT};
use std::sync::Arc;

/// Fixed row height. Three single-line cells plus vertical padding.
///
/// See the module header: this is the row's natural constant height, not a
/// clamp. Changing the cell font sizes without changing this will clip.
pub const ROW_HEIGHT: f32 = 62.0;

const SENDER_SIZE: f32 = 13.0;
const SUBJECT_SIZE: f32 = 13.0;
const SNIPPET_SIZE: f32 = 12.0;
const DATE_WIDTH: f32 = 54.0;

#[derive(Clone)]
pub struct MailListModel {
    store: Arc<dyn MessageStore>,
}

impl MailListModel {
    pub fn new(store: Arc<dyn MessageStore>) -> Self {
        Self { store }
    }
}

impl VirtualListModel for MailListModel {
    fn len(&self) -> usize {
        self.store.len()
    }

    fn row_id(&self, index: usize) -> RowId {
        // CTK's contract is infallible here, but the store is shared and can
        // shrink between the list reading `len` and binding a row. A synthetic
        // id lets the row bind as empty rather than panicking inside the widget
        // on a race the application caused.
        //
        // The synthetic id cannot collide with a real one, and that is
        // enforced rather than assumed. It used to be `u64::MAX - index`, whose
        // safety rested on "no store would issue ids up there" — which is not
        // an invariant, and a collision is silent and nasty: CTK would treat a
        // placeholder and a real message as one row, merging their recycling
        // and selection state. So the id space is split instead: stores own the
        // low half — [`MailRowId`] is the proof, since a trespassing id cannot
        // be constructed — and this line owns the high half
        // (`store::PLACEHOLDER_ID_BIT`). Nothing here depends on `len` staying
        // still.
        self.store
            .row_id(index)
            .map(MailRowId::row_id)
            .unwrap_or(RowId(PLACEHOLDER_ID_BIT | index as u64))
    }

    fn bind(&self, world: &mut World, content: Entity, _index: usize) {
        let row = world
            .get::<ChildOf>(content)
            .expect("CTK bind content has a row parent")
            .parent();
        let metadata = *world
            .get::<VirtualListRow>(row)
            .expect("CTK binds after inserting row metadata");
        // Bind by the id CTK recorded, never by `index`. The two reads are not
        // simultaneous, and the gap is wider than it looks: CTK reads
        // `row_id(index)` inside the realise loop (`virtual_list.rs:1078`) but
        // queues the bind and dispatches every queued job only after the whole
        // reconcile pass has finished (`:684`). A store that inserts or
        // reorders in that window would leave this row *labelled* A while
        // *showing* B — and every action downstream follows the label, so
        // clicking the message you can see would open, mark read or reply to
        // one you cannot. Identity is what the rest of the app acts on, so
        // identity is what the content is fetched by; `index` survives only as
        // the placeholder's serial number.
        let Some(id) = MailRowId::new(metadata.row_id) else {
            // A placeholder row: the store had already shrunk past the index
            // when `row_id` ran. Returning binds it empty rather than stale —
            // CTK despawns the row's children and spawns a fresh `content`
            // before every bind (`virtual_list.rs:1178`), so nothing of the
            // row's previous message survives this return.
            return;
        };
        let Some(summary) = self.store.summary_by_id(id) else {
            return;
        };

        let primary = if metadata.selected {
            tokens::ROW_SELECTED_TEXT
        } else {
            tokens::TEXT
        };
        let secondary = if metadata.selected {
            tokens::ROW_SELECTED_TEXT
        } else {
            tokens::TEXT_DIM
        };
        let weight = if summary.unread {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };

        if let Some(mut node) = world.get_mut::<Node>(content) {
            node.flex_direction = FlexDirection::Column;
            node.padding = UiRect::axes(px(10), px(6));
            node.row_gap = px(2);
        }

        let sender = cell(
            world,
            summary.from.clone(),
            Node {
                min_width: px(0),
                flex_grow: 1.0,
                overflow: Overflow::clip(),
                ..default()
            },
            primary.clone(),
            SENDER_SIZE,
            weight,
        );
        let date = cell(
            world,
            summary.date.clone(),
            Node {
                width: px(DATE_WIDTH),
                min_width: px(DATE_WIDTH),
                justify_content: JustifyContent::FlexEnd,
                ..default()
            },
            secondary.clone(),
            SNIPPET_SIZE,
            weight,
        );
        let header = world
            .spawn((
                Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: px(8),
                    ..default()
                },
                Pickable::IGNORE,
            ))
            .add_children(&[sender, date])
            .id();

        let subject = cell(
            world,
            summary.subject.clone(),
            Node {
                width: percent(100),
                min_width: px(0),
                overflow: Overflow::clip(),
                ..default()
            },
            primary,
            SUBJECT_SIZE,
            weight,
        );
        let snippet = cell(
            world,
            summary.snippet.clone(),
            Node {
                width: percent(100),
                min_width: px(0),
                overflow: Overflow::clip(),
                ..default()
            },
            secondary,
            SNIPPET_SIZE,
            FontWeight::NORMAL,
        );

        world
            .entity_mut(content)
            .add_children(&[header, subject, snippet]);

        let mut accessible = world
            .get_mut::<AccessibilityNode>(content)
            .expect("CTK bind content has accessibility metadata");
        // Read state is spoken, because a screen-reader user cannot see the
        // weight change that carries it visually.
        accessible.set_label(format!(
            "{}{}, from {}, {}. {}",
            if summary.unread { "Unread. " } else { "" },
            summary.subject,
            summary.from,
            summary.date,
            summary.snippet
        ));
    }
}

fn cell(
    world: &mut World,
    text: String,
    node: Node,
    colour: ThemeToken,
    size: f32,
    weight: FontWeight,
) -> Entity {
    world
        .spawn((
            node,
            Text::new(text),
            // Every cell is single-line by design (module header). `no_wrap`
            // plus the parent's `Overflow::clip` is what makes the row's height
            // independent of its content.
            TextLayout::no_wrap(),
            TextFont::from_font_size(size).with_font_weight(weight),
            ThemeTextColor(colour),
            Pickable::IGNORE,
        ))
        .id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FixtureStore;

    #[test]
    fn model_length_tracks_the_store() {
        let store = Arc::new(FixtureStore::synthetic(37));
        let model = MailListModel::new(store);
        assert_eq!(model.len(), 37);
    }

    #[test]
    fn row_ids_are_unique_across_the_model() {
        let store = Arc::new(FixtureStore::synthetic(500));
        let model = MailListModel::new(store);
        let ids: std::collections::HashSet<RowId> =
            (0..model.len()).map(|index| model.row_id(index)).collect();
        assert_eq!(
            ids.len(),
            model.len(),
            "duplicate row ids would corrupt CTK's recycling"
        );
    }

    #[test]
    fn an_out_of_range_index_yields_a_unique_id_rather_than_panicking() {
        let store = Arc::new(FixtureStore::synthetic(4));
        let model = MailListModel::new(store);
        let a = model.row_id(9);
        let b = model.row_id(10);
        assert_ne!(a, b, "the race fallback must not collapse distinct rows");
        assert_ne!(a, model.row_id(0));
        // The property that makes the fallback safe is not "these two differ"
        // — it is that every invented id lands in the half no store may issue,
        // and every store id lands outside it. Checking only the former would
        // pass for `u64::MAX - index`, which is what this replaced.
        assert_ne!(a.0 & PLACEHOLDER_ID_BIT, 0);
        assert_ne!(b.0 & PLACEHOLDER_ID_BIT, 0);
        for index in 0..model.len() {
            assert_eq!(
                model.row_id(index).0 & PLACEHOLDER_ID_BIT,
                0,
                "a real row must never occupy the placeholder half"
            );
        }
    }
}
