//! The measurement the slice was built to take.
//!
//! ## What it drives, and the one thing it cannot
//!
//! Three costs matter and they are measured separately. Two of them are
//! **partial by construction**, and reading them as totals is the single
//! easiest way to draw a false conclusion from this file:
//!
//! - **List recycling**, read from `VirtualList::latency()` — CTK's own
//!   number, but scoped to CTK's own systems: the timer starts at the top of
//!   `reconcile_virtual_lists` and stops at the end of `paint_virtual_rows`
//!   (`ctk/src/virtual_list.rs:669`, `:1243`), both in `Update`. Shaping and
//!   laying out the text nodes it just bound happens in `PostUpdate` and is
//!   *not* in this number. It answers "what does CTK's rebind cost", never
//!   "what does a rebind cost the frame".
//! - **Body swap**, from `reader::ReaderStats` — despawn, sanitise, project,
//!   spawn and flush, timed with exclusive world access so the flush is
//!   inside the measurement. Partial in exactly the same way and for the same
//!   reason.
//! - **Whole frame** — wall clock between `drive` calls. The only total here,
//!   and the only number that can bound the other two.
//!
//! Because of that, a difference between two whole-frame measurements is the
//! only sound way this probe attributes cost: subtracting one partial histogram
//! from a total assigns everything unmeasured — vsync pacing, rendering,
//! unrelated systems — to whatever the subtraction was about.
//!
//! ## Why the difference is taken *inside* one process
//!
//! Differencing two separate runs carries an assumption nobody stated for a
//! while: that two independent processes have the same unmeasured floor —
//! renderer, presentation pacing, scheduling. **That assumption is false often
//! enough to matter.** Two adjacent runs of the identical command have come
//! back at 187.1 ms and 21.1 ms mean, and one at 724.8 ms, on an idle machine,
//! with CTK's own list number steady at ~1 ms and the swap at ~0.7 ms through
//! all of them. Whatever that is, it lives in the part of the frame this probe
//! does not instrument — so a treatment run taken during one and a control run
//! taken outside it would have charged the whole stall to the treatment.
//!
//! Machine load correlates with the same kind of inflation, and it hits both
//! arms rather than adding a constant: paired runs at load average 16–17 read
//! 25.5/37.2, 35.5/49.1 and 582.6/616.7 ms where the same binary at load 2
//! reads 16.0/23.0 ms. That is between a two-thirds and a thirty-fold miss, so
//! there is no correction to apply — only a run to discard.
//!
//! The mechanism is **not established**, and saying so is the point. Load
//! correlates; that is all these runs show. A GPU clamp was suspected and is
//! not supported —
//! `/sys/devices/pci0000:00/0000:00:02.0/tile0/gt0/freq0/throttle/reasons`
//! read `none` when sampled after an inflated run — but a reading taken
//! afterwards cannot rule out throttling *during* it, so this is a lead not
//! pursued rather than a cause eliminated.
//!
//! Either way it argues for the paired form, which is the useful part: both
//! arms live through whatever the machine is doing, and the per-pair *median*
//! survives it — as an empirical result, not a guarantee. The 582.6/616.7 ms run
//! still reported a median pair difference of +6.4 ms, against +6.4 to +7.1 ms
//! on an idle machine. The guarantee it is not: a median tolerates contamination
//! in fewer than *half* the pairs, so on a short run it tolerates very little,
//! which is why [`ProbePlan::parse`] will not admit a paired run below four
//! pairs.
//!
//! A fourth `--probe` field names a second stride, and the run then alternates
//! between the two in **blocks of ten frames**, in an **ABBA** order, and pairs
//! are trimmed from the tail until exactly half ran each arm first. Both arms
//! are measured through one process, one renderer, one presentation clock,
//! interleaved finely enough that a *sustained* stall spans both arms — a
//! single-frame stall lands in one block and one arm, which is what the paired
//! median is there to absorb rather than something the interleaving prevents.
//!
//! Blocks and not single frames, and the reason is the sharpest thing this
//! probe has taught: **under vsync, a frame that overruns displaces its cost
//! onto the next frame.** Alternating every frame therefore hands each arm's
//! overrun to the other one, and the first version of this did exactly that and
//! answered backwards with total confidence — the full-rebind arm 6.6 ms
//! *faster* than the shift arm, 86 of 90 pairs inverted, while the run's
//! aggregate mean sat correctly between the two unpaired runs. Right total,
//! wrong split. [`PAIRED_BLOCK_FRAMES`] carries the detail.
//!
//! The pairing carries its own instrument check, and it is what caught the
//! above: the per-pair differences are kept **signed**, split into how many
//! pairs came back dearer, cheaper and exactly tied, and put through a
//! **two-sided exact sign test** on the untied ones. That test assumes the pair
//! signs are independent draws and they are not — adjacent blocks share one
//! presentation timeline, so a persistent regime correlates them. It is a screen
//! that can say "unproven", not a measurement that can say "proven";
//! [`Paired::sign_test_p`] carries the argument.
//!
//! Every word of that is a correction of the version before it, which counted
//! "inversions" against half the pairs. That rule was wrong three ways, and all
//! three were found by review rather than by it ever looking wrong in a report:
//! it took the *magnitude* of each difference, so a negative pair saturated to
//! zero and vanished into the mean; it treated "at least half inverted" as the
//! null hypothesis when it is equally the signature of a real effect in the
//! opposite direction — running this binary with its arms reversed
//! (`--probe 60:25:0:5`) measured a clean −6.4 ms and printed *"this run shows
//! no difference at all"*; and it scored a tie as agreement, so two identical
//! treatments read as perfectly directional. A one-sided rule cannot be the
//! instrument check for a difference whose sign is a command-line argument.
//!
//! The probe cannot synthesise a *selection*. `VirtualList` exposes
//! `selected_ids`, `is_selected` and `set_selection_mode` but **no programmatic
//! selection setter**, so nothing outside a pointer or key event can move the
//! selection. The probe therefore calls `app::select` directly — the whole of
//! what the selection observer queues, one indirection down. It must stay the
//! whole of it: driving only `swap_body` measured a strict subset of the real
//! path and reported the app as faster than it was.
//!
//! **What that still leaves out, stated rather than glossed:** the list's own
//! selection state never changes during a probe run. `selected_ids` stays
//! empty, so CTK never rebinds the previously-selected row to clear its
//! highlight nor the new one to draw it, and no `VirtualListSelectionChanged`
//! is dispatched. A real arrow-key press pays two extra row rebinds and an
//! observer dispatch that no number in this file contains. Two rebinds against
//! the 24 a full window rebind already costs (`list.rs`) puts the omission at
//! roughly 0.5 ms — small, but it is an omission and not a rounding error, and
//! it cannot be closed from outside CTK. This is the concrete cost of the
//! missing setter, which is why it is recorded as a finding in its own right.
//!
//! That gap is worth naming rather than working around: an app cannot restore
//! its last-read message at startup, cannot implement "next unread", and
//! cannot test selection-driven behaviour headlessly. It is the one API
//! addition this slice actually wants from `virtual_list`, and it is recorded
//! as a finding rather than added here, because adding it is a CTK change with
//! its own review.

use std::time::Instant;

use bevy::app::AppExit;
use bevy::prelude::*;
use ctk::latency::LatencyHistogram;
use ctk::prelude::{virtual_list_scroll_to, Align, VirtualList};

use crate::app::{select, Ui};
use crate::reader::{Corpus, Reader, ReaderStats};
use crate::store::MailRowId;

/// How hard, and for how long, to drive the app.
#[derive(Resource, Clone, Copy, Debug)]
pub struct ProbePlan {
    /// Frames to run before reporting and exiting.
    pub frames: u32,
    /// Model rows advanced per frame. A stride larger than the realised window
    /// guarantees every frame is a full rebind rather than a cheap shift.
    pub stride: usize,
    /// First model row to visit.
    ///
    /// Exists to isolate one body class. `FixtureStore` cycles five bodies by
    /// `position % 5`, so a stride that is a multiple of five pins every frame
    /// to a single fixture: `5:0` is all newsletter, `5:4` is all
    /// 400-paragraph digest. Attributing a latency tail to a body class needs
    /// that; a mixed run can only show that a tail exists.
    pub offset: usize,
    /// A second stride to alternate with, in blocks, within this run.
    ///
    /// Present turns the run into a **paired** measurement (module header):
    /// every second block of [`PAIRED_BLOCK_FRAMES`] frames advances by this
    /// instead of [`stride`](Self::stride), so both treatments share one
    /// process's renderer and presentation floor and a stall lands on both arms
    /// rather than on whichever run happened to catch it.
    ///
    /// Pick the two so they differ in *one* thing. The pairing this slice cares
    /// about is `5` against `25` at offset 0: a 24-row realised window makes 5 a
    /// shift and 25 a full rebind, while both stay multiples of five, so
    /// `FixtureStore`'s `position % 5` keeps the body class pinned across the
    /// alternation. A pairing that also changed the body would be measuring two
    /// differences and attributing them to one.
    pub alt_stride: Option<usize>,
}

impl Default for ProbePlan {
    fn default() -> Self {
        Self {
            frames: 600,
            stride: 37,
            offset: 0,
            alt_stride: None,
        }
    }
}

/// How many consecutive frames one arm holds before the run switches arms.
///
/// **Not one.** Alternating every frame was the first design and it was wrong in
/// a way worth keeping written down, because it looked right and produced a
/// confident, reversed answer.
///
/// Under vsync a frame that overruns its interval presents late, and the frame
/// after it then finds most of an interval already elapsed and completes in what
/// is left. Cost is therefore *displaced onto the neighbour*, and frame-by-frame
/// alternation makes the neighbour the other arm every single time. Measured:
/// `200:5:0:25` frame-alternating reported the full-rebind arm 6.6 ms **faster**
/// than the shift arm, with 86 of 90 pairs inverted — a large, consistent,
/// backwards result. The aggregate mean of that same run, 18.4 ms, sat exactly
/// between the two unpaired runs, which is the giveaway: the run had the right
/// total and the wrong split.
///
/// A block long enough to contain several vsync beats averages the displacement
/// out inside the block instead of across the arms. Ten frames is ten beats at
/// the 60 Hz floor, a sixth of a second — still fine enough that a *sustained*
/// stall spans both arms, which is the whole point of pairing inside one
/// process. A single-frame stall lands in one block and so in one arm; that one
/// is absorbed by taking the per-pair median, not by the interleaving.
const PAIRED_BLOCK_FRAMES: u32 = 10;

/// Frames discarded at the start of each block.
///
/// The displacement above does not vanish at a block boundary, it just stops
/// repeating: the first frame of a block can still carry cost from the last
/// frame of the previous one, which is the other arm. Dropping the first two
/// frames of every block removes that edge without touching the interior, at a
/// cost of a fifth of the samples.
const PAIRED_BLOCK_SETTLE: u32 = 2;

/// Frames a complete block contributes to its mean.
///
/// A block that collected fewer than this was cut short by the end of the run.
/// It is discarded rather than paired: a three-sample block differenced against
/// an eight-sample one is a pair in name only, and it carries the same weight in
/// the sign test as a whole one.
const PAIRED_BLOCK_SAMPLES: u64 = (PAIRED_BLOCK_FRAMES - PAIRED_BLOCK_SETTLE) as u64;

/// Blocks discarded, in their entirety, before any comparison begins.
///
/// Two, not one, because the design below pairs blocks `(2k, 2k+1)` — dropping a
/// whole pair keeps the pairing aligned, and dropping only block 0 would leave
/// block 1 unmatched while still contributing to an arm mean. That unmatched
/// block is not neutral: it is one arm's *first* exposure straight out of
/// warm-up, the block most likely to still hold cold work, and it would land on
/// one side of the headline difference and nowhere in the pairs.
const PAIRED_WARMUP_BLOCKS: u32 = 2;

/// The fewest counterbalanced pairs a paired estimate may rest on.
///
/// Enforced twice, and it has to be. [`ProbePlan::parse`] applies it to the
/// frame count, which is a floor on what the run *asks* for; [`Arms::paired`]
/// applies it to the pairs that actually survived filtering and the balance
/// trim, which is a floor on what the run *got*. A 100-frame run asks for four
/// pairs and can arrive with two — one lost to a short block, one to the trim —
/// and two is exactly the sample size the parser refuses, because a median over
/// two differences is decided outright by one stalled block.
const PAIRED_MIN_PAIRS: usize = 4;

/// The arm a frame belongs to: 0 is the base stride, 1 the alternate.
///
/// The block sequence is **ABBA**, not ABAB, and that is a correctness
/// requirement rather than a flourish. With ABAB every pair runs base-then-
/// alternate, so any drift *within* a pair — a residue of the boundary
/// displacement [`PAIRED_BLOCK_SETTLE`] only partly removes — always pushes the
/// difference the same way and is indistinguishable from the treatment. ABBA
/// alternates the order, so a consistent within-pair drift cancels between pair
/// kinds instead of accumulating into the answer.
///
/// Alternating is not by itself an even split: an odd number of accepted pairs
/// leaves one kind in excess, and then the drift cancels only in proportion —
/// 15 pairs against 14 in the canonical 600-frame run leaves a twenty-ninth of
/// it in the answer. [`Arms::paired`] trims the excess so the split is exactly
/// even; this function only guarantees the alternation it trims.
fn arm_of(frame: u32) -> u32 {
    let block = frame / PAIRED_BLOCK_FRAMES;
    (block % 2) ^ ((block / 2) % 2)
}

impl ProbePlan {
    /// Which stride frame `frame` advances by.
    ///
    /// Derived from [`arm_of`](Self::arm_of), which is the single definition of
    /// which arm a frame belongs to — the stride it runs and the histogram its
    /// interval lands in must never be able to disagree.
    pub fn stride_for(&self, frame: u32) -> usize {
        match self.alt_stride {
            Some(alt) if arm_of(frame) == 1 => alt,
            _ => self.stride,
        }
    }

    /// Parse `--probe [frames[:stride[:offset[:alt_stride]]]]` out of the
    /// command line.
    ///
    /// Returns `Err` for a malformed value rather than silently running the
    /// default: a probe that quietly ignored its own arguments would produce
    /// numbers that do not match what was asked for, which is worse than no
    /// numbers.
    pub fn parse(args: &[String]) -> Result<Option<Self>, String> {
        let Some(position) = args.iter().position(|arg| arg == "--probe") else {
            return Ok(None);
        };
        let Some(value) = args.get(position + 1) else {
            return Ok(Some(Self::default()));
        };
        if value.starts_with("--") {
            return Ok(Some(Self::default()));
        }
        let mut fields = value.split(':');
        let frames = fields.next().unwrap_or_default();
        let stride = fields.next();
        let offset = fields.next();
        let alt_stride = fields.next();
        if let Some(extra) = fields.next() {
            return Err(format!(
                "--probe takes at most frames:stride:offset:alt_stride, \
                 got a fifth field {extra:?}"
            ));
        }
        let frames = frames
            .parse::<u32>()
            .map_err(|_| format!("--probe frames must be a number, got {frames:?}"))?;
        if frames == 0 {
            return Err("--probe frames must be greater than zero".to_string());
        }
        let stride = match stride {
            Some(stride) => stride
                .parse::<usize>()
                .map_err(|_| format!("--probe stride must be a number, got {stride:?}"))?,
            None => Self::default().stride,
        };
        if stride == 0 {
            return Err("--probe stride must be greater than zero".to_string());
        }
        let offset = match offset {
            Some(offset) => offset
                .parse::<usize>()
                .map_err(|_| format!("--probe offset must be a number, got {offset:?}"))?,
            None => Self::default().offset,
        };
        let alt_stride = match alt_stride {
            Some(alt) => Some(
                alt.parse::<usize>()
                    .map_err(|_| format!("--probe alt_stride must be a number, got {alt:?}"))?,
            ),
            None => None,
        };
        if alt_stride == Some(0) {
            return Err("--probe alt_stride must be greater than zero".to_string());
        }
        // Refused rather than accepted as a degenerate pairing. Two identical
        // strides produce two arms of the same treatment, whose difference is
        // pure noise — and a difference of ~0 ms is exactly what someone reads
        // as "the rebind is free". A measurement that answers a question nobody
        // asked, in the shape of an answer to the one they did, is worse than an
        // error.
        if alt_stride == Some(stride) {
            return Err(format!(
                "--probe alt_stride {stride} equals stride: a pair of identical \
                 arms measures nothing"
            ));
        }
        // A paired run spends its first two blocks on warm-up and needs
        // [`PAIRED_MIN_PAIRS`] complete pairs after them to compare anything.
        // Below that the sign test has almost nothing to test and the difference
        // approaches one block against one block, which is a single sample
        // wearing a statistic's clothing — refused rather than printed. (This
        // said "two complete pairs" while the constant said four, from the round
        // that raised it: the prose kept the number the code had left behind.)
        //
        // The count must also be a whole number of blocks. A run ending
        // mid-block leaves a short block that would either be paired against a
        // full one — a pair in name only, carrying equal weight in the sign
        // test — or silently dropped, which makes the frame count a lie about
        // what was measured. Rejecting at parse time is the only place this can
        // be said before the run rather than after it.
        //
        // Eight measured blocks and not four, which is the smallest count that
        // makes the reported median mean anything. The median tolerates
        // contamination in fewer than half the pairs; at two pairs that
        // tolerance is zero — differences of `[6 ms, 1000 ms]` put the median at
        // 503 ms — so a run that short would print a robust-looking number that
        // a single stalled block decides. Four pairs is still a low bar, and the
        // report says how many there were.
        let minimum = (PAIRED_WARMUP_BLOCKS + 2 * PAIRED_MIN_PAIRS as u32) * PAIRED_BLOCK_FRAMES;
        if alt_stride.is_some() && frames < minimum {
            return Err(format!(
                "--probe paired runs need at least {minimum} frames \
                 ({PAIRED_BLOCK_FRAMES}-frame blocks, the first \
                 {PAIRED_WARMUP_BLOCKS} discarded as warm-up), got {frames}"
            ));
        }
        if alt_stride.is_some() && frames % (2 * PAIRED_BLOCK_FRAMES) != 0 {
            return Err(format!(
                "--probe paired runs need a whole number of block pairs: \
                 {frames} is not a multiple of {}",
                2 * PAIRED_BLOCK_FRAMES
            ));
        }
        Ok(Some(Self {
            frames,
            stride,
            offset,
            alt_stride,
        }))
    }
}

/// One block's worth of collected frames: which arm ran it, its mean, and how
/// many frames that mean is over.
///
/// The count is carried because a short block — the run ended inside it — must
/// not be paired against a whole one. Its mean is over fewer frames and its vote
/// in the sign test would weigh the same as a complete block's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    index: u32,
    arm: u32,
    mean_us: u64,
    count: u64,
}

/// One accepted pair: the two arms' block means, and which arm ran first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pair {
    a_mean_us: u64,
    b_mean_us: u64,
    /// True when the alternate arm's block ran before the base arm's.
    ///
    /// Only used to report the counterbalance — that half the pairs ran each
    /// way is the claim [`arm_of`] makes, and a run that failed to deliver it
    /// should say so rather than be trusted.
    alternate_first: bool,
}

impl Pair {
    /// The paired difference, signed. Positive means the alternate arm was
    /// dearer.
    fn difference_us(&self) -> i64 {
        i64::try_from(self.b_mean_us).unwrap_or(i64::MAX)
            - i64::try_from(self.a_mean_us).unwrap_or(i64::MAX)
    }
}

/// What a paired run concluded, and the evidence for it.
struct Paired {
    /// Mean of the base arm's block means over accepted pairs, or `None` when no
    /// counterbalanced estimate survived.
    a_mean_us: Option<f64>,
    /// Mean of the alternate arm's block means over accepted pairs, or `None`.
    b_mean_us: Option<f64>,
    /// Per-pair signed differences, in the order the pairs ran.
    differences_us: Vec<i64>,
    /// Pairs in which the alternate block ran first.
    alternate_first: usize,
    /// Block slots the run should have filled and did not, whether because the
    /// block was short, unmatched, or never reached [`Arms`] at all.
    dropped_blocks: usize,
    /// Pairs dropped to make the order split exactly even.
    unbalanced_pairs: usize,
    /// Pairs discarded because too few survived to support a median.
    short_of_floor: usize,
}

impl Paired {
    fn pairs(&self) -> usize {
        self.differences_us.len()
    }

    /// The notice for a run that lost block slots, or `None` if none went
    /// missing.
    ///
    /// A function rather than an inline `println!` so the condition can be
    /// tested. The version this replaced was unreachable from any test: swapping
    /// its condition for `false` left the whole suite green, which is the same
    /// unpinned-guard shape the survivor floor was caught in.
    ///
    /// The wording is bounded by what `dropped_blocks` actually proves. It says
    /// pairing was incomplete and that local counterbalance *may* have been
    /// lost — not that it *was*. A run that loses only its final short block
    /// keeps every surviving pair adjacent and exactly counterbalanced, so an
    /// unconditional "the split is no longer adjacent" is false in the most
    /// common case, and a warning that overstates is one an operator learns to
    /// discount. It also cannot speak of "the surviving order split" at all when
    /// the floor cleared every pair, so that case is named separately.
    fn retake_notice(&self) -> Option<String> {
        if self.dropped_blocks == 0 {
            return None;
        }
        let plural = if self.dropped_blocks == 1 { "" } else { "s" };
        Some(format!(
            "probe: WARNING {} block slot{plural} produced no pair, so this run did \
             not pair as planned. {} Retake it before quoting the figures above.",
            self.dropped_blocks,
            if self.pairs() == 0 {
                "No pair survived, so there is no estimate to qualify."
            } else {
                "Where the loss falls mid-run the balance trim restores equal \
                 order counts but not adjacency, so the counterbalance may hold \
                 only across the run rather than between neighbouring pairs."
            },
        ))
    }

    /// Pairs where the alternate arm was dearer, cheaper, and exactly equal.
    ///
    /// Ties are counted and reported separately rather than folded into either
    /// side. A tie is not evidence in *either* direction, and silently treating
    /// it as one — which counting only "cheaper" does — makes a run of identical
    /// arms read as perfectly directional.
    fn signs(&self) -> (usize, usize, usize) {
        let dearer = self.differences_us.iter().filter(|d| **d > 0).count();
        let cheaper = self.differences_us.iter().filter(|d| **d < 0).count();
        (dearer, cheaper, self.pairs() - dearer - cheaper)
    }

    /// Two-sided exact sign test on the non-tied pairs.
    ///
    /// `None` when every pair tied, which is the one case with nothing to test.
    /// This replaces a rule that compared inversions against half the pairs and
    /// declared "no difference" — that rule was wrong in both directions: it
    /// read a real effect in the *opposite* sense as no effect at all (proven by
    /// running the arms reversed, where it called a clean −6.4 ms noise), and it
    /// read ties as agreement.
    ///
    /// **What this number is, and what it is not.** It is the exact probability
    /// of a split at least this lopsided from independent fair coins. The pairs
    /// are *not* independent fair coins: they are adjacent blocks in one
    /// process, on one renderer and one presentation clock, in a fixed
    /// deterministic order. A scheduling or thermal regime that persists across
    /// several blocks correlates their signs, and then 28 same-direction pairs
    /// can be one run-level state rather than 28 draws.
    ///
    /// So this is an **exact reference probability under an assumption these
    /// pairs do not satisfy** — and specifically *not* an upper bound, which is
    /// what this comment claimed in the draft that first admitted the problem.
    /// A bound needs a known direction, and serial dependence has none here:
    /// positive correlation makes the independent-draw figure too small
    /// (anti-conservative), negative correlation makes it too large. It is
    /// therefore a screen and not a measurement, useful mainly in the direction
    /// where it says *less*: a value above 0.05 means the *split* does not clear
    /// the screen even granting independence. That is weaker than "carries no
    /// weight", which this comment used to say, and the floor makes the gap
    /// concrete — the smallest run this accepts is four pairs, where four out of
    /// four in one direction is p=0.125. Plainly directional, and it does not
    /// clear the screen; at that sample size nothing can.
    ///
    /// And only ever the split: the test counts signs and discards magnitudes,
    /// so a high p is never evidence that the two arms cost the same.
    /// Differences of +100, +100, −1, −1 are two dearer against two cheaper,
    /// p=1, beside a mean of +49.5. What actually carries the direction in
    /// `list.rs` is replication — three idle runs, one with the arms reversed,
    /// one under thirty-fold load, all agreeing — which is a between-run
    /// argument this within-run statistic cannot make.
    fn sign_test_p(&self) -> Option<f64> {
        let (dearer, cheaper, _) = self.signs();
        let n = dearer + cheaper;
        if n == 0 {
            return None;
        }
        let k = dearer.min(cheaper);
        // Exact binomial tail at p = 0.5, summed *downward* from the `k`th term.
        //
        // The obvious form — start at `0.5^n` and climb — is wrong for a long
        // run, and wrong in the worst direction. `0.5^n` underflows to zero past
        // n = 1074, which is reachable (`--probe 40000:5:0:25` is about six
        // minutes at 60 Hz), and every later term is then zero too, so the
        // function returns **p = 0 for an even split**: maximal confidence from
        // the one dataset that carries none.
        //
        // Summing downward from the `k`th term avoids it, because that term is
        // the largest one in the tail and stays representable at any n — at the
        // even split it is about `sqrt(2 / (pi * n))`. `binomial_pmf_half`
        // computes it without ever forming `C(n, k)`, which does overflow.
        let mut term = binomial_pmf_half(n, k);
        let mut tail = term;
        for i in (1..=k).rev() {
            // pmf(i-1) / pmf(i) = i / (n - i + 1).
            term *= i as f64 / (n - i + 1) as f64;
            tail += term;
        }
        // `min(1.0)` is not papering over an error: at `k = n/2` the lower tail
        // exceeds one half — it includes the whole central term — so doubling it
        // genuinely exceeds 1, and 1 is the right answer. A two-sided p of 1 says
        // the split is exactly what a fair coin gives, which is precisely the
        // even-split case.
        Some((2.0 * tail).min(1.0))
    }
}

/// `C(n, k) * 0.5^n` — one binomial probability at p = 0.5.
///
/// Computed by interleaving the `2^-n` with the coefficient's own factors and
/// keeping the running value at or below one, so neither the coefficient
/// (which overflows `f64` past n ≈ 1029) nor the power (which underflows past
/// n = 1074) is ever formed on its own. The result is bounded by 1 by
/// definition, so the interleaved form is the only one that stays in range at
/// every step.
fn binomial_pmf_half(n: usize, k: usize) -> f64 {
    let mut value = 1.0f64;
    let mut halves = n;
    for i in 1..=k {
        value *= (n - k + i) as f64 / i as f64;
        while value > 1.0 && halves > 0 {
            value *= 0.5;
            halves -= 1;
        }
    }
    for _ in 0..halves {
        value *= 0.5;
    }
    value
}

/// The two interleaved arms of a paired run, and the instrument check on them.
struct Arms {
    /// One entry per completed block, in the order the blocks ran.
    ///
    /// The unit of comparison is the *block*, not the frame — see
    /// [`PAIRED_BLOCK_FRAMES`] for why a frame is the wrong unit and produced a
    /// reversed answer. Blocks are paired at report time.
    blocks: Vec<Block>,
    /// The block being filled: its index, the running microsecond sum, and the
    /// count of frames in that sum.
    current: Option<(u32, u64, u64)>,
    /// How many frames the *plan* asks for, independent of what arrived.
    ///
    /// Carried rather than derived, because deriving the run's end from `blocks`
    /// makes it mean whatever the run managed to record. A block whose frames
    /// were all settle frames, or a tail that never ran at all, leaves no
    /// entry — so the highest stored index moves down with the loss, `expected`
    /// moves down with it, and the missing slots score as zero dropped. The
    /// accounting then reads "nothing went missing" precisely when something
    /// did.
    ///
    /// A frame count and not a block index, which is a distinction with teeth.
    /// Holding only the planned last *block* made the boundary authoritative at
    /// block granularity: an `Arms::new(95)` fed frames 0..100 accepted 95–99,
    /// because they fall inside planned block 9 — completing a block the plan
    /// left short, producing four sound-looking pairs, and leaving the overrun
    /// counter at zero. Every admitted paired plan is a multiple of twice the
    /// block length, so the two granularities agree today; the exact endpoint is
    /// what makes that a fact about the parser rather than a coincidence the
    /// estimator depends on.
    planned_frames: u32,
    /// Frames offered past the planned last frame, refused rather than recorded.
    ///
    /// Zero on every run the driver can currently produce. It is reported rather
    /// than asserted because the report is what an operator reads, and a silent
    /// count of nothing costs nothing.
    beyond_plan: u64,
}

impl Arms {
    fn new(frames: u32) -> Self {
        Self {
            blocks: Vec::new(),
            current: None,
            beyond_plan: 0,
            planned_frames: frames,
        }
    }

    /// The last block index the plan reaches, or `None` for a plan of no frames.
    ///
    /// `checked_sub` because a zero-frame plan reaches no block at all, and
    /// `(0 - 1) / BLOCK` would name block 429496729 as planned. That case is
    /// unreachable through the parser, which refuses a zero-frame run, but it is
    /// also the case where the old code was worst: a zero-frame plan produced no
    /// planned block, which then read as "no plan" and accepted every frame
    /// offered.
    fn planned_last_block(&self) -> Option<u32> {
        self.planned_frames
            .checked_sub(1)
            .map(|last| last / PAIRED_BLOCK_FRAMES)
    }

    /// The overrun notice, or `None` if the driver stayed inside its plan.
    ///
    /// A function for the same reason [`Paired::retake_notice`] is one: the
    /// version this replaced was an inline `if` in `report` that no test
    /// reached, so the counter could increment correctly while nothing printed
    /// it. Catching that shape once and then reintroducing it in the fix is
    /// exactly the pattern this review loop keeps finding.
    ///
    /// Worded so the count is the object rather than the subject — "refused N
    /// frames that arrived" and not "N frames arrived … and were refused" —
    /// because the second form needs the verb to agree as well as the noun, and
    /// the version this replaced inflected only the noun: at `beyond_plan == 1`
    /// it printed "1 frame … were refused". The singular case is the one an
    /// operator is most likely to meet, an overrun of a single frame being the
    /// smallest one possible.
    fn overrun_notice(&self) -> Option<String> {
        (self.beyond_plan > 0).then(|| {
            format!(
                "probe: WARNING refused {} frame{} that arrived past the planned \
                 last frame. The driver overran its own plan; the paired figures \
                 cover the plan only, while the whole-frame histogram above does \
                 not.",
                self.beyond_plan,
                if self.beyond_plan == 1 { "" } else { "s" },
            )
        })
    }

    /// Record one whole-frame interval against the arm of the frame that
    /// *caused* it.
    ///
    /// `frame` is the frame the interval covers, which is the previous frame,
    /// not the one now starting — the interval ends when the next `drive`
    /// begins, so it holds the previous frame's downstream layout and
    /// presentation. Charging it to the frame now starting would offset every
    /// sample by one frame; at a block boundary that is an arm boundary, which
    /// is the one way this pairing could be confidently wrong.
    fn record(&mut self, frame: u32, sample: std::time::Duration) {
        let block = frame / PAIRED_BLOCK_FRAMES;
        // The plan is authoritative in both directions, not just downwards, and
        // to the frame rather than to the block. Carrying the planned endpoint
        // only to *extend* it with whatever arrived made it a high-water mark: an
        // `Arms::new(100)` fed frames 0..140 accepted four extra blocks, reported
        // six balanced pairs and zero dropped, and silently measured forty frames
        // nobody asked for. Comparing blocks instead of frames then left a
        // sub-block seam: `Arms::new(95)` fed 0..100 accepted 95–99 as part of
        // planned block 9, filling out a block the plan left short. Refused and
        // counted rather than absorbed — a driver that overruns its plan is a bug
        // to see, and the alternative to counting is deciding it never happened.
        if frame >= self.planned_frames {
            self.beyond_plan = self.beyond_plan.saturating_add(1);
            return;
        }
        if self.current.map(|(index, _, _)| index) != Some(block) {
            self.finish();
            self.current = Some((block, 0, 0));
        }
        // The first blocks are warm-up in their entirety. The whole-frame
        // histogram still sees these frames — "what did this run cost" must
        // include its own start-up — but a comparison must not, because first
        // shaping, first window realisation and cold caches all land in one arm.
        if block < PAIRED_WARMUP_BLOCKS {
            return;
        }
        if frame % PAIRED_BLOCK_FRAMES < PAIRED_BLOCK_SETTLE {
            return;
        }
        if let Some((_, sum, count)) = self.current.as_mut() {
            // Saturating because a stalled frame is exactly when this must not
            // wrap, and `u64` microseconds overflow only past half a million
            // years of run time — the saturation is a guard, not a live path.
            *sum = sum.saturating_add(sample.as_micros().min(u64::MAX as u128) as u64);
            *count += 1;
        }
    }

    /// Close the block being filled, if it collected anything.
    ///
    /// Must be called before reading [`blocks`](Self::blocks) or the last block
    /// of the run is silently missing — which for a 200-frame run is one pair in
    /// nine, quietly dropped from the answer.
    fn finish(&mut self) {
        if let Some((block, sum, count)) = self.current.take() {
            // `checked_div` rather than a count guard: a block that collected
            // nothing — every frame in it a settle frame, or the whole warm-up
            // block — has no mean, and "no mean" and "a mean of zero" must not
            // become the same entry in `blocks`, because a zero would pair as a
            // free block.
            if let Some(mean_us) = sum.checked_div(count) {
                self.blocks.push(Block {
                    index: block,
                    arm: arm_of(block * PAIRED_BLOCK_FRAMES),
                    mean_us,
                    count,
                });
            }
        }
    }

    /// Pair blocks `(2k, 2k+1)` and summarise.
    ///
    /// Pairing is by block *index*, not by position in `blocks`, so a block that
    /// failed to collect anything cannot silently shift its neighbours into the
    /// wrong partners.
    ///
    /// Three things a pair must satisfy before it counts, each of which was a
    /// defect in the version this replaced:
    ///
    /// - **Both blocks complete.** A block cut short by the end of the run has
    ///   its mean over fewer frames but the same weight in the sign test.
    /// - **One block of each arm.** Guaranteed by [`arm_of`]'s ABBA sequence,
    ///   checked anyway, because the alternative to checking is an answer that
    ///   differences an arm against itself.
    /// - **Membership decides the headline means too.** An unmatched block used
    ///   to reach the printed arm mean but no pair; since the unmatched block is
    ///   whichever one the run ended next to, that put an arbitrary block on one
    ///   side of the difference and nowhere on the other.
    /// - **The order split is made exact, not left near-exact.** ABBA alternates
    ///   pair order, so an *odd* number of accepted pairs leaves one extra pair
    ///   of one kind — 15 against 14 in the canonical 600-frame run — and a
    ///   dropped middle block can skew it further. That residue does not cancel;
    ///   it is a fraction of the order effect ABBA exists to remove, left in the
    ///   answer while the prose claims it was taken out. Pairs are trimmed from
    ///   the tail until the two kinds are equal, and the count trimmed is
    ///   reported rather than absorbed.
    fn paired(&self) -> Paired {
        let mut pairs: Vec<Pair> = Vec::new();
        let mut matched = 0usize;
        let by_index = |index: u32| self.blocks.iter().find(|block| block.index == index);
        // Walks to the last block index rather than stopping at the first
        // missing one. A gap mid-run is not reachable today — a block collects
        // nothing only when the run ends inside it, which is the last block —
        // but "stop at the first gap" would silently truncate the run to the
        // blocks before it and report the survivors as the whole answer, which
        // is the failure mode that costs the most to notice.
        //
        // The endpoint is the plan's alone, with no fallback to what arrived.
        // `record` refuses every frame past the plan, so nothing stored can sit
        // above it, and reaching for the maximum of the two would only make an
        // overrun *invisible* — the expanded endpoint moves `expected` up in
        // step with the extra blocks, so the run reports zero dropped while
        // measuring frames outside the experiment.
        let last = self.planned_last_block();
        let mut index = PAIRED_WARMUP_BLOCKS;
        while Some(index) <= last {
            let (first, second) = (by_index(index), by_index(index + 1));
            index += 2;
            let (Some(first), Some(second)) = (first, second) else {
                continue;
            };
            if first.count < PAIRED_BLOCK_SAMPLES || second.count < PAIRED_BLOCK_SAMPLES {
                continue;
            }
            let (a, b) = match (first.arm, second.arm) {
                (0, 1) => (first, second),
                (1, 0) => (second, first),
                // Same arm twice: `arm_of` is broken, and differencing an arm
                // against itself would report a floor as a treatment effect.
                _ => continue,
            };
            matched += 2;
            pairs.push(Pair {
                a_mean_us: a.mean_us,
                b_mean_us: b.mean_us,
                alternate_first: first.arm == 1,
            });
        }
        // Remove the *latest* pair of whichever order is in excess, until the
        // two orders are equally represented.
        //
        // The guarantee this gives, and the one it does not. It gives: the pair
        // removed is chosen by position and order alone, never by its
        // difference, so the trim cannot be steered towards the answer. It does
        // not give a prefix of the run — an earlier comment here claimed that
        // and was wrong. With a gap the accepted orders can run
        // alternate, alternate, base, alternate; the excess is removed from the
        // end, then from the *interior*, keeping a later pair. Counts come out
        // equal, which is what the counterbalance needs, but the kept pairs are
        // no longer contiguous, so a drift that varies over the run is only
        // cancelled globally and not locally. A gap is unreachable today (a
        // block collects nothing only when the run ends inside it) — this is
        // what happens if that changes, stated rather than assumed away.
        let mut unbalanced_pairs = 0usize;
        loop {
            let alternate = pairs.iter().filter(|pair| pair.alternate_first).count();
            let base = pairs.len() - alternate;
            if alternate == base {
                break;
            }
            let excess = alternate > base;
            let Some(position) = pairs
                .iter()
                .rposition(|pair| pair.alternate_first == excess)
            else {
                break;
            };
            pairs.remove(position);
            unbalanced_pairs += 1;
        }
        // The parser's four-pair floor is a floor on what the run *set out* to
        // measure; this is the floor on what it actually got. They are not the
        // same number the moment a block goes missing: a valid 100-frame run
        // starts with four pairs, loses one to a short block, loses another to
        // the balance trim, and reports a two-pair median — the exact sample
        // size the parser refuses as non-robust. Enforcing it only at parse time
        // would let the estimator print the number the parser exists to prevent.
        //
        // Counted separately from the balance trim rather than added to it: one
        // is "this pair had no counterpart", the other "there were not enough
        // pairs left to say anything", and a report that merged them would
        // describe a run that measured nothing as a run that was tidied.
        let mut short_of_floor = 0usize;
        if pairs.len() < PAIRED_MIN_PAIRS {
            short_of_floor = pairs.len();
            pairs.clear();
        }
        // `None` and not a zero. With no pairs there is no estimate, and a
        // `0.0 ms` arm mean beside a `+0.0 ms` median reads as a measured
        // absence of difference — a fabricated finding, in the one situation
        // where the run established nothing at all.
        let means = (!pairs.is_empty()).then(|| {
            let count = pairs.len() as f64;
            (
                pairs.iter().map(|pair| pair.a_mean_us as f64).sum::<f64>() / count,
                pairs.iter().map(|pair| pair.b_mean_us as f64).sum::<f64>() / count,
            )
        });
        // Counted against the block slots the run should have produced, not
        // against the blocks that reached `Arms`. A block that never arrived at
        // all is absent from `blocks`, so differencing against `blocks.len()`
        // scored four missing blocks as "0 dropped" — silence in exactly the
        // case that most needs the noise. `last` comes from the plan for the
        // same reason: derived from the stored blocks it shrinks to fit the
        // loss, and a missing *tail* — the one shape the driver can actually
        // produce, by ending inside a block — stays invisible.
        let expected = last.map_or(0, |last| {
            (last + 1).saturating_sub(PAIRED_WARMUP_BLOCKS) as usize
        });
        Paired {
            a_mean_us: means.map(|(a, _)| a),
            b_mean_us: means.map(|(_, b)| b),
            alternate_first: pairs.iter().filter(|pair| pair.alternate_first).count(),
            differences_us: pairs.iter().map(Pair::difference_us).collect(),
            dropped_blocks: expected.saturating_sub(matched),
            unbalanced_pairs,
            short_of_floor,
        }
    }
}

#[derive(Resource)]
struct ProbeRun {
    plan: ProbePlan,
    frame: u32,
    cursor: usize,
    started: Option<Instant>,
    /// Start of the previous `drive` call, for the whole-frame histogram.
    last_frame: Option<Instant>,
    /// Wall clock between consecutive `drive` calls.
    ///
    /// Load-bearing, not decoration: the swap and list histograms only see the
    /// work their own systems do. Everything a swap *causes* — text layout,
    /// UI layout, render extraction over the newly spawned body — lands in
    /// later schedules and would be invisible. The first run of this probe
    /// showed 3 ms of measured work inside a 108 ms frame, so reporting the
    /// measured work alone would have been an outright false result.
    frames: LatencyHistogram,
    /// Frames that ended without the requested message in the reading pane.
    ///
    /// Three causes are merged here — the store had no id at the cursor's
    /// index, the message was gone by the time it was read, or the reader
    /// declined the swap — and the counter deliberately does not distinguish
    /// them (see `report`). An earlier version of this comment named only the
    /// first, which made the other two read as an empty mailbox.
    ///
    /// Counted and reported rather than ignored: a skipped frame is a frame of
    /// the run that did *less* than the run claims to measure, so silently
    /// skipping would make the app look faster — which is the one direction a
    /// probe must never be wrong in.
    skipped: u32,
    /// The id the previous frame selected, so an unchanged cursor is not
    /// replayed as a fresh selection.
    ///
    /// This is what makes the held-body control runs mean what they say. CTK
    /// emits `VirtualListSelectionChanged` only when the selected *set* changes
    /// (`virtual_list.rs:1338`), and the probe stands in for that observer — so
    /// a probe that calls `select` on every frame regardless is not simulating
    /// the app, it is simulating an app nobody can drive. It mattered the moment
    /// the reader stopped short-circuiting on an unchanged id: a stride that
    /// wraps to the same row every frame silently turned "one body swap, then
    /// idle" into two hundred, and the runs that isolate the *list's* cost by
    /// holding the body still would have been measuring two hundred body
    /// materialisations instead.
    ///
    /// Note what this compares and what it does not: two *selected identities*,
    /// not two message revisions. It is deliberately an id comparison and not an
    /// index one — a store that moved a message to a different row has not
    /// changed the selection, and CTK would not emit an event for it either. The
    /// reader has no such short-circuit and must not grow one, because the
    /// question it faces is the other one entirely: *which version* of the
    /// message the pane is showing, which an identity cannot answer. See
    /// `store::MailRowId`.
    last_selected: Option<MailRowId>,
    /// Frames whose cursor landed on the row already selected.
    ///
    /// Reported, because a run of 200 frames that drove 1 selection and a run
    /// that drove 200 are entirely different experiments and the output has to
    /// say which one happened.
    repeats: u32,
    /// The paired arms, present only when the plan named an alternate stride.
    arms: Option<Arms>,
}

pub fn install(app: &mut App, plan: ProbePlan) {
    app.insert_resource(plan)
        .insert_resource(ProbeRun {
            plan,
            frame: 0,
            cursor: plan.offset,
            started: None,
            last_frame: None,
            frames: LatencyHistogram::default(),
            skipped: 0,
            last_selected: None,
            repeats: 0,
            arms: plan.alt_stride.map(|_| Arms::new(plan.frames)),
        })
        .add_systems(Update, drive);
}

/// Exclusive, because the body swap it drives needs `&mut World`.
fn drive(world: &mut World) {
    let Some(run) = world.get_resource::<ProbeRun>() else {
        return;
    };
    let (plan, frame, cursor) = (run.plan, run.frame, run.cursor);
    let Some(&Ui { list, .. }) = world.get_resource::<Ui>() else {
        return;
    };

    let now = Instant::now();
    if let Some(mut run) = world.get_resource_mut::<ProbeRun>() {
        if frame == 0 {
            run.started = Some(now);
        }
        if let Some(previous) = run.last_frame.replace(now) {
            let sample = now.duration_since(previous);
            run.frames.record(sample);
            // Charged to the frame that just ended, not the one starting — see
            // `Arms::record`. `frame` is the index of the frame about to run, so
            // the interval belongs to `frame - 1`, and `frame` is at least 1
            // here because `last_frame` was already set.
            if let Some(arms) = run.arms.as_mut() {
                arms.record(frame.saturating_sub(1), sample);
            }
        }
    }

    if frame >= plan.frames {
        // The block in flight is closed before anything reads it. Reporting
        // first would drop the run's last block — one pair in nine on a
        // 200-frame run, and silently.
        if let Some(mut run) = world.get_resource_mut::<ProbeRun>() {
            if let Some(arms) = run.arms.as_mut() {
                arms.finish();
            }
        }
        report(world);
        world.write_message(AppExit::Success);
        // Remove the driver so a frame that runs after the exit message does
        // not report twice.
        world.remove_resource::<ProbeRun>();
        return;
    }

    let model_len = world
        .get::<VirtualList>(list)
        .map(|list| list.model_len())
        .unwrap_or(0);
    if model_len == 0 {
        // Same rule as the missing-id path below, and for the same reason: an
        // empty model is a condition to *report*, not one to wait out. Returning
        // without advancing left `--probe` running until the shell killed it,
        // which is a hang whose only symptom is a diagnostic tool that never
        // answers.
        if let Some(mut run) = world.get_resource_mut::<ProbeRun>() {
            run.frame += 1;
            run.skipped += 1;
        }
        return;
    }
    let index = cursor % model_len;

    {
        let mut commands = world.commands();
        virtual_list_scroll_to(&mut commands, list, index, Align::Start);
    }
    world.flush();
    // Selection cannot be driven programmatically (module header), so the app
    // is advanced through the same entry point the observer queues — all of
    // it, not just the body swap.
    //
    // The id comes from the store rather than being built from `index`. That is
    // not pedantry: `RowId(index)` happens to be right for the fixture corpus
    // and would silently stop being right for any store whose ids are not
    // positions, at which point the probe would measure repeated selections of
    // whichever messages did happen to line up — and report the app as faster
    // than it is, which is the one failure a probe must not have.
    let id = world
        .get_resource::<Corpus>()
        .and_then(|corpus| corpus.0.row_id(index));
    // A skip is any frame that did not end with the requested message on
    // screen, which is one question wider than "did the index resolve". The id
    // can resolve and the *message* still be gone by the time `select` reads it
    // — a shrink between the two reads — and that frame does a fraction of the
    // work of a real one. Counting only the first case let the cheaper frame
    // into the mean unremarked, which is the direction of error a probe must
    // not have.
    let last_selected = world
        .get_resource::<ProbeRun>()
        .and_then(|run| run.last_selected);
    // A cursor that lands on the already-selected row is *not* a selection, and
    // driving `select` anyway would be the probe inventing an event the widget
    // never emits. See `ProbeRun::last_selected` — this is what keeps the
    // held-body runs held.
    let repeat = id.is_some() && id == last_selected;
    let shown = match id {
        Some(id) if !repeat => select(world, id),
        // Nothing to do and nothing wrong: the requested message is already the
        // one on screen, so the frame is neither a swap nor a skip.
        Some(_) => true,
        None => false,
    };

    // The frame advances either way. Returning early without advancing would
    // make a store that has no id at `index` — a shrink between `model_len` and
    // this read — spin the probe until the shell's timeout instead of finishing
    // its run, and a diagnostic tool that hangs on the condition it exists to
    // observe is worse than one that reports the condition.
    if let Some(mut run) = world.get_resource_mut::<ProbeRun>() {
        run.frame += 1;
        run.cursor = cursor.wrapping_add(plan.stride_for(frame));
        if repeat {
            run.repeats += 1;
        } else if shown {
            run.last_selected = id;
        } else {
            run.skipped += 1;
        }
    }
}

/// The list section's inputs, read out of the world rather than borrowed from
/// it, so the formatter below is a function of plain data.
struct ListSection {
    latency: String,
    realised: usize,
    model_len: usize,
}

/// Everything the report prints, gathered in one place.
///
/// The split exists because extracting `retake_notice` and `overrun_notice` as
/// testable helpers proved their *conditions and wording* and nothing about
/// whether the report emits them: deleting both call sites left all 78 tests
/// green, and so did deleting the skipped-frame warning. A helper tested in
/// isolation pins the string it builds, not the line the operator reads. With
/// the formatting a pure function of this struct, a test can assert the whole
/// output and a dropped line fails it.
struct ReportData<'a> {
    run: &'a ProbeRun,
    /// Passed in rather than read from `run.started`, which keeps the output
    /// deterministic under test — the one field that would otherwise differ
    /// between two runs of identical data.
    wall: std::time::Duration,
    list: Option<ListSection>,
    reader: Option<&'a ReaderStats>,
}

fn report(world: &World) {
    let Some(run) = world.get_resource::<ProbeRun>() else {
        return;
    };
    let wall = run
        .started
        .map(|started| started.elapsed())
        .unwrap_or_default();
    let list = world
        .get_resource::<Ui>()
        .and_then(|ui| world.get::<VirtualList>(ui.list))
        .map(|list| ListSection {
            latency: list.latency().summary(),
            realised: list.realised_range().len(),
            model_len: list.model_len(),
        });
    let reader = world.get_resource::<Reader>().map(|reader| &reader.stats);
    for line in report_lines(&ReportData {
        run,
        wall,
        list,
        reader,
    }) {
        println!("{line}");
    }
}

fn report_lines(data: &ReportData) -> Vec<String> {
    let ReportData {
        run,
        wall,
        list,
        reader,
    } = data;
    let wall = *wall;
    let mut lines = Vec::new();
    let stride_note = match run.plan.alt_stride {
        Some(alt) => format!("stride {} alternating with {alt}", run.plan.stride),
        None => format!("stride {}", run.plan.stride),
    };
    lines.push(format!(
        "probe: {} frames, {stride_note}, offset {}, wall {:.2}s",
        run.plan.frames,
        run.plan.offset,
        wall.as_secs_f64()
    ));
    // Not a warning: a stride that wraps onto the same row every frame is how
    // the held-body control runs are *built*. It is printed because a run of 200
    // frames that drove one selection and one that drove 200 are different
    // experiments, and the numbers below are only interpretable against which of
    // the two happened.
    lines.push(format!(
        "probe: {} of {} frames re-selected the row already selected, so drove no swap",
        run.repeats, run.plan.frames
    ));
    lines.push(format!(
        "probe: whole frame {} {}",
        run.frames.summary(),
        tail_note(&run.frames)
    ));
    if let Some(arms) = &run.arms {
        let alt = run.plan.alt_stride.unwrap_or(run.plan.stride);
        let paired = arms.paired();
        // Every figure derived from the pairs is printed, or withheld, together.
        // Formatting them independently is how a run that kept no pairs comes to
        // print `0.0 ms` beside a `+0.0 ms` median and read as a *measured*
        // absence of difference — a fabricated finding in the one case where the
        // run established nothing.
        let (a_mean, b_mean, difference) = match (paired.a_mean_us, paired.b_mean_us) {
            (Some(a), Some(b)) => (
                format!("{:.1} ms", a / 1000.0),
                format!("{:.1} ms", b / 1000.0),
                format!("{:+.1} ms", (b - a) / 1000.0),
            ),
            _ => ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        };
        lines.push(format!(
            "probe: paired in one process, {PAIRED_BLOCK_FRAMES}-frame ABBA blocks \
             (first {PAIRED_WARMUP_BLOCKS} discarded, {PAIRED_BLOCK_SETTLE} settle \
             frames each) · stride {} {a_mean} · stride {alt} {b_mean} · \
             difference of means {difference} over {} matched pairs \
             ({} ran the stride-{alt} block first, {} block{} dropped, \
             {} pair{} trimmed to balance the order, {} discarded as too few \
             to support a median)",
            run.plan.stride,
            paired.pairs(),
            paired.alternate_first,
            paired.dropped_blocks,
            if paired.dropped_blocks == 1 { "" } else { "s" },
            paired.unbalanced_pairs,
            if paired.unbalanced_pairs == 1 {
                ""
            } else {
                "s"
            },
            paired.short_of_floor,
        ));
        lines.extend(paired.retake_notice());
        lines.extend(arms.overrun_notice());
        // The per-pair number, and the reason it is printed next to the
        // difference of means rather than instead of it: they answer the same
        // question through different sensitivities to a stall. A run where they
        // disagree is a run to retake, and neither alone would say so.
        //
        // Signed, and both directions named. The version this replaced took the
        // magnitude and called every negative pair "inverted", which silently
        // assumed the alternate arm was the dearer one — an assumption the
        // command line can reverse, and which turned a clean result into
        // "no difference" when it was.
        let (dearer, cheaper, tied) = paired.signs();
        let verdict = match paired.sign_test_p() {
            // Zero pairs and every-pair-tied both leave nothing to test, and
            // they mean opposite things: one is a run that measured nothing,
            // the other a run that measured no difference. Naming them the
            // same way would let a run with no usable pairs read as a
            // confident finding of equality.
            None if paired.pairs() == 0 => {
                " · no counterbalanced pair survived, so this run measured nothing".to_string()
            }
            None => " · every pair tied, so there is nothing to test".to_string(),
            Some(p) => format!(
                " · two-sided sign test p={p:.3}{}",
                if p > 0.05 {
                    " — the split does not cross the 0.05 screen under the \
                     independent fair-sign reference model, so read the \
                     difference above as unproven. Not the same as no evidence, \
                     and not a finding of equality: the smallest admitted run is \
                     four pairs, and four out of four in one direction is p=0.125 \
                     — directional, just too small to cross. The test reads signs \
                     only, so differences of +100, +100, −1, −1 give p=1 beside a \
                     mean of +49.5"
                } else {
                    " — consistent with an effect, but the pairs are adjacent \
                     blocks on one timeline rather than independent draws, and \
                     serial dependence can move this figure either way, so it \
                     is a reference probability and not a bound; the direction \
                     rests on replication across runs"
                }
            ),
        };
        let median = match median_us(&paired.differences_us) {
            Some(median) => format!("{:+.1} ms", median / 1000.0),
            None => "n/a".to_string(),
        };
        lines.push(format!(
            "probe: per-pair difference (stride {alt} minus stride {}) \
             median {median} · {dearer} pair{} dearer, {cheaper} cheaper, {tied} tied{verdict}",
            run.plan.stride,
            if dearer == 1 { "" } else { "s" },
        ));
    }
    if run.skipped > 0 {
        // Loud, because these frames are in the mean above and did less work
        // than the rest of the run. Any conclusion drawn from a run with skips
        // is a conclusion about a partly-idle app.
        // The count deliberately does not name a cause. A frame lands here when
        // the store had no id at the cursor, *or* the message was gone by the
        // time it was read, *or* the reader declined the swap — and an earlier
        // version asserted the first of those, which made the other two look
        // like an empty mailbox. Splitting them needs per-reason counters; until
        // a run actually reports skips, one honest number beats three
        // speculative ones.
        lines.push(format!(
            "probe: WARNING {} of {} frames put nothing new in the reading pane. \
             The frame numbers above include them and are therefore optimistic.",
            run.skipped, run.plan.frames
        ));
    }

    if let Some(list) = list {
        lines.push(format!(
            "probe: virtual-list CTK Update systems only {} · realised {} of {}",
            list.latency, list.realised, list.model_len
        ));
    }

    if let Some(stats) = reader {
        lines.push(format!(
            "probe: body swaps {} · whole swap {} {} · sanitise alone {}",
            stats.swaps,
            stats.swap.summary(),
            tail_note(&stats.swap),
            stats.sanitize.summary()
        ));
        // Means, not quantiles, and stated as a bound rather than a
        // decomposition. Both histograms are exact in the mean and coarse in
        // the tail, and the swap is a *part* of the frame it sits in — so the
        // only claim the arithmetic supports is a ceiling on what removing the
        // swap entirely could ever recover. What the remainder consists of is
        // not knowable from one run: it holds downstream layout, but also
        // presentation pacing, rendering and every other system. Attributing
        // it needs a second run that differs in one variable (see the module
        // header); this line must not pretend to do that.
        //
        // The share is computed from *totals*, not from one mean divided by
        // the other. Those two agree only when every frame swapped, and the
        // runs that matter most do not: `200:50000:4` swaps once in 200
        // frames, where mean-over-mean prints 8.4/18.7 = 45% for work that is
        // 0.2% of the run. Dividing a per-swap mean by a per-frame mean is a
        // unit error that happens to look right in the common case, which is
        // the kind that survives review.
        let swap_mean = stats.swap.mean_us();
        let frame_mean = run.frames.mean_us();
        let swap_total = swap_mean.saturating_mul(stats.swap.count());
        let frame_total = frame_mean.saturating_mul(run.frames.count());
        if frame_total > 0 {
            let share = (swap_total as f64 / frame_total as f64) * 100.0;
            lines.push(format!(
                "probe: mean frame {:.1} ms; the swap is {:.1} ms per swap, {swaps} of them, \
                 {share:.1}% of the run's total frame time — deleting the swap outright could \
                 recover at most that {share:.1}%, and the remainder is unattributed here, not \
                 measured as layout",
                frame_mean as f64 / 1000.0,
                swap_mean as f64 / 1000.0,
                swaps = stats.swap.count(),
            ));
        }
    }
    lines
}

/// Median of the signed per-pair differences, in microseconds.
///
/// The median and not the mean, because this is the companion to a sign test:
/// both are statements about where the pairs sit rather than about their total,
/// and a stalled block should not move the number the sign test is qualifying as
/// far as it moves the mean. *As far as*, not "at all": a median is unmoved by
/// contamination in fewer than half the pairs and decided by it at half, so on a
/// four-pair run — the shortest [`ProbePlan::parse`] admits — two stalled pairs
/// carry it. The run's *mean* difference is still printed, one line up, from the
/// arm means; where the two disagree the run is one to retake.
///
/// Computed here rather than through [`LatencyHistogram`] because that type is
/// unsigned and coarse, and the sign of a pair is the whole point.
///
/// `None` on an empty slice, where it used to return `0.0`. Zero is a *value* —
/// "the two arms differed by nothing" — and no differences at all is the absence
/// of one; printing the first for the second turns a run that kept no pairs into
/// a confident finding of equality.
fn median_us(differences: &[i64]) -> Option<f64> {
    if differences.is_empty() {
        return None;
    }
    let mut sorted = differences.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[middle] as f64
    } else {
        // Averaged in `f64` rather than `(a + b) / 2` in `i64`: the integer form
        // truncates toward zero, which biases a negative median upward and a
        // positive one downward — a systematic pull toward "no effect" in
        // exactly the number that is meant to detect one.
        (sorted[middle - 1] as f64 + sorted[middle] as f64) / 2.0
    })
}

/// Flag a summary whose quantiles have saturated at the histogram's maximum.
///
/// `LatencyHistogram::quantile_us` returns the true maximum once a quantile
/// lands in the open-ended bucket past the last ceiling
/// (`ctk/src/latency.rs:81`). That is honest for a single outlier and badly
/// misleading for a run that *lives* there: 199 frames at 130 ms and one at
/// 1050 ms would report `p50≤1050ms`, and a whole table of such rows reads as
/// a typical-case measurement when it is a worst case.
///
/// **Both printed quantiles are checked, not just p50.** Checking p50 alone was
/// the first version of this and it is the more dangerous half to miss: a run
/// with a well-behaved median and a saturated p99 — 98 frames at 16 ms, one at
/// 130 ms, one at 1050 ms — is exactly the shape where p99 is the number a
/// reader trusts, and it would have printed `p99≤1050ms` silently. p50
/// saturating implies the run *lives* in the tail, which is loud on its own.
///
/// The trigger is `quantile == max`, which also fires benignly when every
/// sample sits at one closed bucket's ceiling — so the note claims only what is
/// true in both cases (a quantile has landed on the maximum and cannot be read
/// as a distribution) and prints the mean, which is computed from the running
/// sum and is exact either way.
fn tail_note(histogram: &LatencyHistogram) -> String {
    // An empty histogram answers 0 to every quantile *and* to `max_us`, so the
    // equality below holds vacuously and the note would fire on a run that
    // recorded nothing — announcing a saturated tail where there is no
    // distribution at all. A single sample is fine: its quantile really is its
    // maximum, and the note is true.
    if histogram.count() == 0 {
        return String::new();
    }
    let saturated = [0.5, 0.99]
        .into_iter()
        .any(|q| histogram.quantile_us(q) == histogram.max_us());
    if !saturated {
        return String::new();
    }
    format!(
        "(a quantile has landed on max — read the exact mean {:.1} ms instead)",
        histogram.mean_us() as f64 / 1000.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn absent_flag_means_no_probe() {
        assert!(ProbePlan::parse(&args(&["--other"]))
            .expect("parses")
            .is_none());
    }

    #[test]
    fn bare_flag_uses_the_default_plan() {
        let plan = ProbePlan::parse(&args(&["--probe"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.frames, ProbePlan::default().frames);
        assert_eq!(plan.stride, ProbePlan::default().stride);
    }

    #[test]
    fn frames_and_stride_are_both_accepted() {
        let plan = ProbePlan::parse(&args(&["--probe", "120:5"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.frames, 120);
        assert_eq!(plan.stride, 5);
        assert_eq!(plan.offset, ProbePlan::default().offset);
    }

    #[test]
    fn an_offset_pins_the_body_class() {
        let plan = ProbePlan::parse(&args(&["--probe", "120:5:4"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.frames, 120);
        assert_eq!(plan.stride, 5);
        assert_eq!(plan.offset, 4);
        // Zero is a real offset, not "unset": it pins fixture 0.
        let zero = ProbePlan::parse(&args(&["--probe", "120:5:0"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(zero.offset, 0);
    }

    #[test]
    fn a_following_flag_is_not_read_as_a_value() {
        let plan = ProbePlan::parse(&args(&["--probe", "--verbose"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.frames, ProbePlan::default().frames);
    }

    #[test]
    fn malformed_values_are_refused_not_defaulted() {
        assert!(ProbePlan::parse(&args(&["--probe", "many"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "0"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:0"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:x"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:5:x"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:5:0:x"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:5:0:0"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:5:0:25:9"])).is_err());
    }

    #[test]
    fn an_alternate_stride_makes_the_run_paired() {
        let plan = ProbePlan::parse(&args(&["--probe", "200:5:0:25"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.stride, 5);
        assert_eq!(plan.alt_stride, Some(25));
        // The arm switches on the *block*, not the frame. Pinned here because a
        // regression to frame alternation is invisible in every number the probe
        // prints except the inversion count, and it answers backwards.
        assert_eq!(plan.stride_for(0), 5);
        assert_eq!(plan.stride_for(PAIRED_BLOCK_FRAMES - 1), 5);
        assert_eq!(plan.stride_for(PAIRED_BLOCK_FRAMES), 25);
        // ABBA, so block 2 stays on the alternate stride and block 3 returns to
        // the base. An ABAB regression passes the two assertions above and fails
        // here, which is the point of testing block 2 at all.
        assert_eq!(plan.stride_for(2 * PAIRED_BLOCK_FRAMES), 25);
        assert_eq!(plan.stride_for(3 * PAIRED_BLOCK_FRAMES), 5);
        assert_eq!(plan.stride_for(4 * PAIRED_BLOCK_FRAMES), 5);
    }

    #[test]
    fn a_paired_run_too_short_to_hold_two_pairs_is_refused() {
        assert!(ProbePlan::parse(&args(&["--probe", "20:5:0:25"])).is_err());
        // The unpaired form has no such floor: a short run of one treatment is
        // a short run, not a comparison that cannot be made.
        assert!(ProbePlan::parse(&args(&["--probe", "20:5:0"])).is_ok());
    }

    #[test]
    fn a_paired_run_ending_mid_pair_is_refused_rather_than_truncated() {
        // A run that stops inside a block leaves a short block, whose mean is
        // over fewer frames but which would vote in the sign test like a whole
        // one. Measured: `75:5:0:25` produced a three-sample block paired
        // against an eight-sample one and counted equally.
        assert!(ProbePlan::parse(&args(&["--probe", "75:5:0:25"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "70:5:0:25"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "110:5:0:25"])).is_err());
        assert!(ProbePlan::parse(&args(&["--probe", "100:5:0:25"])).is_ok());
        // 80 frames parsed until the minimum was raised: it yields three pairs,
        // and a three-pair median is decided by two stalled blocks.
        assert!(ProbePlan::parse(&args(&["--probe", "80:5:0:25"])).is_err());
        // Unpaired runs keep every frame count: there is no pair geometry to
        // violate, and the existing tables were taken at 200 and 600 frames.
        assert!(ProbePlan::parse(&args(&["--probe", "75:5:0"])).is_ok());
    }

    #[test]
    fn an_unpaired_plan_uses_one_stride_for_every_frame() {
        let plan = ProbePlan::parse(&args(&["--probe", "200:25:0"]))
            .expect("parses")
            .expect("probe requested");
        assert_eq!(plan.alt_stride, None);
        assert_eq!(plan.stride_for(0), 25);
        assert_eq!(plan.stride_for(1), 25);
    }

    #[test]
    fn two_identical_arms_are_refused_rather_than_measured() {
        // A degenerate pairing reports ~0 ms of difference, which reads exactly
        // like "the rebind is free" — the wrong answer in the shape of the right
        // one.
        assert!(ProbePlan::parse(&args(&["--probe", "200:25:0:25"])).is_err());
    }

    /// Drive `frames` intervals into a fresh [`Arms`], costing `per_arm[arm]`
    /// milliseconds per frame, and close the last block.
    fn arms_over(frames: u32, per_arm: [u64; 2]) -> Arms {
        let mut arms = Arms::new(frames);
        for frame in 0..frames {
            let cost = per_arm[arm_of(frame) as usize];
            arms.record(frame, std::time::Duration::from_millis(cost));
        }
        arms.finish();
        arms
    }

    #[test]
    fn arms_are_switched_on_block_boundaries_not_every_frame() {
        // The failure this pins answered *backwards* on real hardware: with
        // frame alternation, a frame that overruns hands its cost to the next
        // frame, which is always the other arm. If this regresses, every number
        // stays plausible and the sign flips.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES, [10, 30]);
        let collected = u64::from(PAIRED_BLOCK_FRAMES - PAIRED_BLOCK_SETTLE);
        assert!(
            arms.blocks.iter().all(|block| block.count == collected),
            "every collected block holds a block's frames minus its settle frames"
        );
        let paired = arms.paired();
        assert_eq!(paired.a_mean_us, Some(10_000.0));
        assert_eq!(paired.b_mean_us, Some(30_000.0));
    }

    #[test]
    fn the_block_sequence_is_abba_so_pair_order_is_counterbalanced() {
        // ABAB would make every pair base-then-alternate, and any residual
        // boundary carryover would then push every difference the same way and
        // be indistinguishable from the treatment. Half the pairs must run each
        // way round.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES, [10, 30]);
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 4, "blocks 2-3, 4-5, 6-7, 8-9");
        assert_eq!(
            paired.alternate_first, 2,
            "exactly half the pairs run the alternate arm first"
        );
        // And the counterbalance must not cost the estimate its sign.
        assert_eq!(paired.signs(), (4, 0, 0));
        assert_eq!(paired.unbalanced_pairs, 0, "an even count needs no trim");
    }

    #[test]
    fn an_odd_pair_count_is_trimmed_to_an_exactly_even_order_split() {
        // ABBA alternates the order; it does not by itself *balance* it. The
        // canonical 600-frame run yields 29 pairs — 15 one way, 14 the other —
        // and that lone extra pair carries a twenty-ninth of the very order
        // effect the counterbalance exists to remove, while the prose claims it
        // was removed. Reviewed and accepted as a real residue, not a rounding
        // one, because it does not shrink with run length: it is one pair
        // whatever the length, but so is the bias it leaves.
        let arms = arms_over(12 * PAIRED_BLOCK_FRAMES, [10, 30]);
        let paired = arms.paired();
        assert_eq!(
            paired.pairs(),
            4,
            "10 measured blocks make 5 pairs, 3 of them alternate-first"
        );
        assert_eq!(paired.unbalanced_pairs, 1);
        assert_eq!(
            paired.alternate_first, 2,
            "the trim is what makes the split exact, not approximate"
        );
    }

    #[test]
    fn a_dropped_middle_block_cannot_leave_the_order_split_skewed() {
        // The trim must survive a gap, not just an odd tail: a block that
        // collects nothing removes one pair from the middle, and if that pair
        // was the only one of its kind the remainder is lopsided in a way no
        // "is the count even" check would catch.
        //
        // Long enough that the survivors clear [`PAIRED_MIN_PAIRS`]. At twelve
        // blocks the gap and the trim between them leave two pairs, the floor
        // clears those, and the equal-split assertion below degenerates to
        // `0 == 0` — a test that passes because nothing survived to be
        // unequal, which is not the property it names.
        let mut arms = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]);
        // Blocks 4 and 5 form one pair; drop it and 8 pairs remain, 5 of one
        // kind and 3 of the other.
        arms.blocks.retain(|block| block.index != 4);
        let paired = arms.paired();
        let alternate = paired.alternate_first;
        assert!(
            paired.pairs() >= PAIRED_MIN_PAIRS,
            "the survivors must clear the floor or this proves nothing"
        );
        assert_eq!(
            alternate,
            paired.pairs() - alternate,
            "the two orders are represented equally whatever went missing"
        );
        assert!(paired.unbalanced_pairs > 0, "a trim was needed here");
        assert!(
            paired.a_mean_us.is_some() && paired.b_mean_us.is_some(),
            "a gapped run with enough survivors still reports an estimate"
        );
    }

    #[test]
    fn too_few_surviving_pairs_are_discarded_rather_than_averaged() {
        // The floor is on what the run *got*, so the fixture has to be a plan the
        // parser would *admit*, then degraded — a 60-frame `Arms` proves only the
        // parser's floor a second time, since no paired run that short can reach
        // the estimator at all. This is the exact minimum plan, losing two blocks
        // to leave two balanced pairs: the sample size at which the median's
        // contamination tolerance is zero, reached from a run that asked for
        // enough.
        let minimum = (PAIRED_WARMUP_BLOCKS + 2 * PAIRED_MIN_PAIRS as u32) * PAIRED_BLOCK_FRAMES;
        assert!(
            ProbePlan::parse(&["--probe".to_string(), format!("{minimum}:5:0:25")])
                .expect("the minimum plan is accepted")
                .is_some(),
            "the fixture must be a plan the parser admits"
        );
        let mut arms = arms_over(minimum, [10, 30]);
        assert_eq!(arms.blocks.len(), 8, "blocks 2 through 9");
        // Blocks 4 and 6 fall in different pairs and are of opposite order, so
        // losing both leaves the two survivors already balanced — the floor is
        // then the only thing that can take them.
        arms.blocks
            .retain(|block| block.index != 4 && block.index != 6);
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 0, "two pairs is below the floor");
        assert_eq!(
            paired.short_of_floor, 2,
            "and the two are reported, not hidden"
        );
        assert_eq!(
            paired.unbalanced_pairs, 0,
            "they were balanced; the floor took them"
        );
        assert_eq!(paired.a_mean_us, None);
        assert_eq!(paired.b_mean_us, None);
        assert_eq!(
            median_us(&paired.differences_us),
            None,
            "no median may be fabricated from an empty set"
        );
        assert_eq!(paired.sign_test_p(), None);
    }

    #[test]
    fn a_run_that_lost_a_block_slot_is_marked_for_retake() {
        // The notice had no test at all: swapping its condition for `false` left
        // every other test green, so the one thing telling an operator not to
        // quote a broken run could have been deleted silently.
        let clean = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]);
        assert_eq!(clean.paired().dropped_blocks, 0);
        assert_eq!(
            clean.paired().retake_notice(),
            None,
            "a run that lost nothing must not be marked"
        );

        let mut gapped = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]);
        gapped.blocks.retain(|block| block.index != 4);
        let paired = gapped.paired();
        let notice = paired.retake_notice().expect("a slot went missing");
        assert!(notice.contains("Retake"), "the notice must say what to do");
        assert!(
            paired.pairs() > 0 && notice.contains("may hold"),
            "with survivors it says adjacency may be lost, not that it was"
        );

        // The floor can clear every pair, and then there is no surviving split
        // to describe at all — the branch that would have talked about one.
        let minimum = (PAIRED_WARMUP_BLOCKS + 2 * PAIRED_MIN_PAIRS as u32) * PAIRED_BLOCK_FRAMES;
        let mut emptied = arms_over(minimum, [10, 30]);
        emptied
            .blocks
            .retain(|block| block.index != 4 && block.index != 6);
        let paired = emptied.paired();
        assert_eq!(paired.pairs(), 0);
        assert!(
            paired
                .retake_notice()
                .expect("slots went missing")
                .contains("no estimate to qualify"),
            "with no survivors it must not describe a surviving order split"
        );
    }

    #[test]
    fn frames_past_the_planned_last_block_are_refused_not_absorbed() {
        // The plan is authoritative in both directions. Extending the endpoint to
        // whatever arrived made an overrun invisible: the extra blocks pair,
        // `expected` rises in step, and the run reports zero dropped while
        // measuring frames outside the experiment.
        let planned = 10 * PAIRED_BLOCK_FRAMES;
        let mut arms = Arms::new(planned);
        for frame in 0..planned + 4 * PAIRED_BLOCK_FRAMES {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        arms.finish();
        assert_eq!(
            arms.blocks.iter().map(|block| block.index).max(),
            Some(9),
            "no block past the plan may be stored"
        );
        assert_eq!(
            arms.paired().pairs(),
            4,
            "the plan's four pairs, and no more"
        );
        assert_eq!(
            arms.beyond_plan,
            u64::from(4 * PAIRED_BLOCK_FRAMES),
            "the overrun is counted rather than absorbed"
        );
        assert!(
            arms.overrun_notice()
                .expect("an overrun must be reported, not merely counted")
                .contains("overran its own plan"),
            "counting an overrun that nothing prints is the same as not noticing"
        );
    }

    #[test]
    fn the_plan_ends_at_a_frame_not_at_a_block_boundary() {
        // The seam a block-granularity check left open: frames 115..119 are
        // outside a 115-frame plan but inside planned block 11, so a
        // `block > planned` comparison accepted them — filling out a block the
        // plan had left short, turning it into a complete-looking pair member,
        // and leaving the overrun counter at zero so nothing warned. Long enough
        // that the survivors clear the floor, or the floor rather than the seam
        // would be what fails.
        let mut arms = Arms::new(115);
        for frame in 0..120 {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        arms.finish();
        assert_eq!(
            arms.beyond_plan, 5,
            "frames 115 through 119 are outside the plan"
        );
        let last_block = arms
            .blocks
            .iter()
            .find(|block| block.index == 11)
            .expect("block 11 collected its in-plan frames");
        assert_eq!(
            last_block.count,
            u64::from(PAIRED_BLOCK_FRAMES - PAIRED_BLOCK_SETTLE) - 5,
            "the short block stays short rather than being completed from outside"
        );
        assert!(
            last_block.count < PAIRED_BLOCK_SAMPLES,
            "and is therefore refused as a pair member"
        );
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 4, "blocks 2-3, 4-5, 6-7 and 8-9");
        assert!(
            paired.retake_notice().is_some(),
            "the pair the short block belongs to is a loss the operator must see"
        );
    }

    #[test]
    fn a_plan_of_no_frames_accepts_nothing() {
        // The sharpest form of the same defect. A zero-frame plan reaches no
        // block, which the earlier `Option<u32>` endpoint expressed as `None` —
        // indistinguishable from "no plan", so every frame offered was accepted.
        let mut arms = Arms::new(0);
        for frame in 0..4 * PAIRED_BLOCK_FRAMES {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        arms.finish();
        assert!(arms.blocks.is_empty(), "a plan of nothing measures nothing");
        assert_eq!(arms.beyond_plan, u64::from(4 * PAIRED_BLOCK_FRAMES));
        assert_eq!(arms.paired().pairs(), 0);
    }

    #[test]
    fn a_missing_block_is_counted_whether_it_falls_mid_run_or_at_the_end() {
        // Both halves matter, and the trailing half is the one the driver can
        // actually produce: a run that ends inside its last block leaves no
        // entry for it. Counting against the highest *stored* index scored that
        // as nothing dropped, because the loss moved the endpoint with it.
        let mut arms = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]);
        assert_eq!(arms.blocks.len(), 18, "blocks 2 through 19");
        arms.blocks.retain(|block| block.index != 19);
        assert_eq!(
            arms.paired().dropped_blocks,
            2,
            "block 19 never arrived and block 18 has no partner: two slots, no pair"
        );

        let mut arms = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]);
        arms.blocks
            .retain(|block| block.index != 4 && block.index != 19);
        assert_eq!(
            arms.paired().dropped_blocks,
            4,
            "a mid-run gap and a missing tail are both counted, and separately"
        );
    }

    #[test]
    fn a_lone_unbalanced_pair_is_trimmed_to_nothing_rather_than_reported() {
        // One pair cannot be counterbalanced — its order effect is carried
        // whole — so the trim takes it and the run reports no pairs. That is the
        // intended answer and not a degenerate one: `parse` will not admit a run
        // this short, so reaching here means a caller built `Arms` directly, and
        // a single uncounterbalanced difference presented as a result is exactly
        // what the ABBA design exists to refuse.
        let arms = arms_over(4 * PAIRED_BLOCK_FRAMES, [10, 30]);
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 0);
        assert_eq!(paired.unbalanced_pairs, 1);
        assert_eq!(
            paired.sign_test_p(),
            None,
            "and the report must not call this an even split"
        );
    }

    #[test]
    fn an_interval_is_charged_to_the_frame_that_ended_it() {
        // `drive` passes `frame - 1`, because the interval it just measured
        // covers the frame that ended, not the one starting. Pinned through the
        // block boundary, which is where the off-by-one becomes an arm error:
        // the last frame of block 2 must not land in block 3's arm.
        let boundary = 3 * PAIRED_BLOCK_FRAMES - 1;
        let mut arms = Arms::new(boundary + 1);
        arms.record(boundary, std::time::Duration::from_millis(30));
        arms.finish();
        let [block] = arms.blocks[..] else {
            panic!("one block collected");
        };
        assert_eq!(block.index, 2);
        assert_eq!(
            block.arm, 1,
            "the last frame of an alternate block belongs to the alternate arm"
        );
    }

    #[test]
    fn the_warm_up_blocks_reach_neither_arm() {
        let mut arms = Arms::new(PAIRED_WARMUP_BLOCKS * PAIRED_BLOCK_FRAMES);
        for frame in 0..PAIRED_WARMUP_BLOCKS * PAIRED_BLOCK_FRAMES {
            // A cost that would dominate every later block if it leaked.
            arms.record(frame, std::time::Duration::from_millis(500));
        }
        arms.finish();
        assert!(
            arms.blocks.is_empty(),
            "warm-up must not produce a block at all"
        );
        assert_eq!(arms.paired().pairs(), 0);
    }

    #[test]
    fn a_whole_pair_is_discarded_as_warm_up_leaving_no_unmatched_block() {
        // Discarding only block 0 left block 1 in an arm mean but in no pair —
        // and block 1 is one arm's first exposure straight out of warm-up, the
        // block most likely to still hold cold work. It landed on one side of
        // the headline difference and nowhere on the other.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES, [10, 30]);
        assert_eq!(arms.blocks.len(), 8, "blocks 2 through 9");
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 4);
        assert_eq!(
            paired.dropped_blocks, 0,
            "every collected block belongs to a pair"
        );
    }

    #[test]
    fn a_short_final_block_is_dropped_rather_than_paired() {
        // `parse` refuses these frame counts, so this can only arise from a
        // caller building `Arms` directly — but the estimator must not depend on
        // the parser for its correctness. Measured before the fix: `75:5:0:25`
        // paired a three-sample block against an eight-sample one and gave it a
        // full vote.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES + PAIRED_BLOCK_SETTLE + 1, [10, 30]);
        let paired = arms.paired();
        assert_eq!(
            paired.pairs(),
            4,
            "blocks 2-9 make four pairs; block 10 is short"
        );
        assert_eq!(paired.dropped_blocks, 1);
        assert_eq!(
            paired.a_mean_us,
            Some(10_000.0),
            "the short block reached no arm"
        );
    }

    #[test]
    fn the_difference_keeps_its_sign_when_the_alternate_arm_is_the_cheaper_one() {
        // The defect this pins was found by running the real binary with the
        // arms reversed (`--probe 60:25:0:5`): it measured a clean −6.4 ms and
        // reported "no difference at all", because the magnitude saturated at
        // zero and every pair counted as an inversion against an assumption the
        // command line had just reversed.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES, [30, 10]);
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 4);
        assert_eq!(paired.a_mean_us, Some(30_000.0));
        assert_eq!(paired.b_mean_us, Some(10_000.0));
        assert_eq!(
            median_us(&paired.differences_us),
            Some(-20_000.0),
            "the alternate arm being cheaper is a negative difference, not a zero"
        );
        assert_eq!(paired.signs(), (0, 4, 0));
    }

    #[test]
    fn identical_arms_read_as_tied_rather_than_as_a_direction() {
        // Ties used to increment the pair count but neither side, so a run of
        // two identical treatments printed "0 of n inverted" — the exact shape
        // of a perfectly directional result.
        let arms = arms_over(10 * PAIRED_BLOCK_FRAMES, [17, 17]);
        let paired = arms.paired();
        assert_eq!(paired.pairs(), 4);
        assert_eq!(paired.signs(), (0, 0, 4));
        assert_eq!(
            paired.sign_test_p(),
            None,
            "every pair tied, so there is nothing to test — not a p of zero"
        );
    }

    #[test]
    fn the_sign_test_is_two_sided_and_answers_the_same_either_way_round() {
        // Reversing which arm is dearer must not change the strength of the
        // evidence, only its direction. A one-sided rule fails this.
        let dearer = arms_over(20 * PAIRED_BLOCK_FRAMES, [10, 30]).paired();
        let cheaper = arms_over(20 * PAIRED_BLOCK_FRAMES, [30, 10]).paired();
        // Nine pairs are measured and the ninth is trimmed to leave the order
        // split exactly even.
        assert_eq!(dearer.pairs(), 8);
        assert_eq!(dearer.unbalanced_pairs, 1);
        assert_eq!(dearer.sign_test_p(), cheaper.sign_test_p());
        // Eight pairs all falling the same way: 2 × 0.5^8.
        let p = dearer.sign_test_p().expect("not every pair tied");
        assert!((p - 2.0 * 0.5f64.powi(8)).abs() < 1e-12, "p was {p}");
    }

    #[test]
    fn an_even_split_of_pairs_is_reported_as_unproven() {
        // Two identical treatments split their pairs evenly, and the report must
        // say the difference is unproven rather than print a mean as a finding.
        let mut arms = Arms::new(10 * PAIRED_BLOCK_FRAMES);
        for frame in 0..10 * PAIRED_BLOCK_FRAMES {
            let block = frame / PAIRED_BLOCK_FRAMES;
            // Alternate which arm is dearer from pair to pair, so the signs
            // split 2/2 while neither arm is uniformly faster.
            let alternate_dearer = (block / 2).is_multiple_of(2);
            let dear = (arm_of(frame) == 1) == alternate_dearer;
            arms.record(
                frame,
                std::time::Duration::from_millis(if dear { 30 } else { 10 }),
            );
        }
        arms.finish();
        let paired = arms.paired();
        assert_eq!(paired.signs(), (2, 2, 0), "two pairs each way, none tied");
        let p = paired.sign_test_p().expect("no pair is tied");
        assert!(
            p > 0.05,
            "an even split must not read as evidence, p was {p}"
        );
    }

    #[test]
    fn the_sign_test_survives_a_run_long_enough_to_underflow_the_naive_form() {
        // `0.5^n` is zero in `f64` past n = 1074, and the first version summed
        // the tail upward from it. Every term was then zero, so an *even split*
        // — the dataset carrying no evidence at all — returned p = 0, maximal
        // confidence. Reachable: `--probe 40000:5:0:25` is about six minutes at
        // 60 Hz.
        let even = Paired {
            a_mean_us: Some(0.0),
            b_mean_us: Some(0.0),
            differences_us: (0..4000).map(|i| if i % 2 == 0 { 1 } else { -1 }).collect(),
            alternate_first: 0,
            dropped_blocks: 0,
            unbalanced_pairs: 0,
            short_of_floor: 0,
        };
        assert_eq!(even.signs(), (2000, 2000, 0));
        assert_eq!(
            even.sign_test_p(),
            Some(1.0),
            "an even split of 4000 pairs is exactly what a fair coin gives"
        );

        // And the other end still answers: all one way must stay significant
        // rather than falling out of range in the other direction.
        let decided = Paired {
            differences_us: vec![1; 4000],
            ..even
        };
        assert_eq!(decided.signs(), (4000, 0, 0));
        assert_eq!(decided.sign_test_p(), Some(0.0));
    }

    #[test]
    fn the_binomial_term_matches_hand_arithmetic() {
        // Pinned against values that can be checked by hand, because every
        // guard in `sign_test_p` is downstream of this one function.
        assert!((binomial_pmf_half(4, 2) - 6.0 / 16.0).abs() < 1e-15);
        assert!((binomial_pmf_half(4, 0) - 1.0 / 16.0).abs() < 1e-15);
        assert!((binomial_pmf_half(4, 4) - 1.0 / 16.0).abs() < 1e-15);
        assert!((binomial_pmf_half(9, 0) - 0.5f64.powi(9)).abs() < 1e-15);
        // The even split of a large n, where the naive form is zero: the central
        // term tends to sqrt(2 / (pi * n)).
        let central = binomial_pmf_half(4000, 2000);
        let expected = (2.0 / (std::f64::consts::PI * 4000.0)).sqrt();
        assert!(
            (central - expected).abs() < expected * 1e-3,
            "central term {central} against {expected}"
        );
    }

    #[test]
    fn the_last_block_is_not_dropped() {
        // `finish` is called from `drive` before `report`. Forgetting it loses
        // the final block, and the loss is silent.
        let mut arms = Arms::new(3 * PAIRED_BLOCK_FRAMES);
        for frame in 0..3 * PAIRED_BLOCK_FRAMES {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        let before = arms.blocks.len();
        arms.finish();
        assert_eq!(
            arms.blocks.len(),
            before + 1,
            "the block in flight must be closed before anything reads the run"
        );
    }

    fn plan_of(frames: u32, alt_stride: Option<usize>) -> ProbePlan {
        ProbePlan {
            frames,
            stride: 5,
            offset: 0,
            alt_stride,
        }
    }

    /// The operator's output over hand-built state.
    ///
    /// Everything below asserts on this rather than on the helpers that build
    /// the individual strings, because a helper test proves the string and not
    /// the line: with `retake_notice` and `overrun_notice` each directly tested,
    /// deleting *both* call sites in the report still left the whole suite
    /// green, and so did deleting the skipped-frame warning.
    fn report_over(plan: ProbePlan, arms: Option<Arms>, skipped: u32) -> Vec<String> {
        report_over_with(plan, arms, skipped, None, None, 10)
    }

    fn report_over_with(
        plan: ProbePlan,
        arms: Option<Arms>,
        skipped: u32,
        list: Option<ListSection>,
        reader: Option<&ReaderStats>,
        frame_ms: u64,
    ) -> Vec<String> {
        let mut frames = LatencyHistogram::default();
        for _ in 0..plan.frames {
            frames.record(std::time::Duration::from_millis(frame_ms));
        }
        let run = ProbeRun {
            plan,
            frame: plan.frames,
            cursor: plan.offset,
            started: None,
            last_frame: None,
            frames,
            skipped,
            last_selected: None,
            repeats: 0,
            arms,
        };
        report_lines(&ReportData {
            run: &run,
            wall: std::time::Duration::from_millis(1500),
            list,
            reader,
        })
    }

    #[test]
    fn a_clean_paired_run_reports_five_lines_and_warns_about_nothing() {
        let frames = 10 * PAIRED_BLOCK_FRAMES;
        let lines = report_over(
            plan_of(frames, Some(25)),
            Some(arms_over(frames, [10, 30])),
            0,
        );
        assert_eq!(
            lines.len(),
            5,
            "header, repeats, whole frame, paired block, per-pair — and nothing else: {lines:#?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("WARNING")),
            "a run that stayed inside its plan and lost no block has nothing to warn about"
        );
        assert!(lines[3].contains("0 blocks dropped"));
        assert!(lines[3].contains("difference of means +20.0 ms over 4 matched pairs"));
        assert!(lines[4].contains("4 pairs dearer, 0 cheaper, 0 tied"));
    }

    #[test]
    fn the_report_emits_the_overrun_notice_it_is_given() {
        let planned = 10 * PAIRED_BLOCK_FRAMES;
        let mut arms = Arms::new(planned);
        for frame in 0..planned + 40 {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        arms.finish();
        let lines = report_over(plan_of(planned, Some(25)), Some(arms), 0);
        assert_eq!(
            lines.len(),
            6,
            "the five clean lines plus the overrun: {lines:#?}"
        );
        // Between the paired summary and the per-pair line, because it
        // qualifies both and an operator reading top-down must meet it before
        // the numbers it qualifies.
        assert_eq!(
            lines[4],
            "probe: WARNING refused 40 frames that arrived past the planned last \
             frame. The driver overran its own plan; the paired figures cover the \
             plan only, while the whole-frame histogram above does not."
        );
    }

    #[test]
    fn an_overrun_of_one_frame_reads_as_one_frame() {
        // The plural arm is the easy one to get right and the singular arm is
        // the one an operator meets first, a one-frame overrun being the
        // smallest there is. The wording this pins replaced "1 frame ... were
        // refused", which only a whole-line assertion catches: every
        // count-and-condition assertion on the helper passed over it.
        let planned = 10 * PAIRED_BLOCK_FRAMES;
        let mut arms = Arms::new(planned);
        for frame in 0..=planned {
            arms.record(frame, std::time::Duration::from_millis(10));
        }
        arms.finish();
        let lines = report_over(plan_of(planned, Some(25)), Some(arms), 0);
        assert_eq!(
            lines[4],
            "probe: WARNING refused 1 frame that arrived past the planned last \
             frame. The driver overran its own plan; the paired figures cover the \
             plan only, while the whole-frame histogram above does not."
        );
    }

    #[test]
    fn the_report_emits_the_retake_notice_with_and_without_survivors() {
        // Blocks lost mid-run, with pairs left over: the estimate stands but is
        // qualified.
        let frames = 20 * PAIRED_BLOCK_FRAMES;
        let mut arms = arms_over(frames, [10, 30]);
        arms.blocks
            .retain(|block| block.index != 4 && block.index != 19);
        let lines = report_over(plan_of(frames, Some(25)), Some(arms), 0);
        assert_eq!(
            lines.len(),
            6,
            "the five clean lines plus the retake: {lines:#?}"
        );
        assert!(lines[4].starts_with("probe: WARNING 4 block slots produced no pair"));
        assert!(
            lines[4].contains("the balance trim restores equal order counts but not adjacency"),
            "a survivor-bearing run is qualified, not withdrawn: {}",
            lines[4]
        );

        // Every pair broken: there is no estimate left to qualify, and the
        // figures above must read `n/a` rather than a measured zero.
        let frames = 6 * PAIRED_BLOCK_FRAMES;
        let mut arms = arms_over(frames, [10, 30]);
        arms.blocks
            .retain(|block| block.index != 3 && block.index != 5);
        let lines = report_over(plan_of(frames, Some(25)), Some(arms), 0);
        assert!(lines[3].contains("stride 5 n/a · stride 25 n/a"));
        assert!(lines[3].contains("difference of means n/a over 0 matched pairs"));
        assert!(lines[4].contains("No pair survived, so there is no estimate to qualify"));
        assert!(
            lines[5].contains("no counterbalanced pair survived, so this run measured nothing"),
            "and the verdict must not read as a measured equality: {}",
            lines[5]
        );
    }

    #[test]
    fn the_report_emits_the_skipped_frame_warning() {
        // Reachable on real hardware and never yet reported by a run, which is
        // exactly the branch that rots: an empty mailbox skips every frame, and
        // the numbers above stay plausible while measuring a partly-idle app.
        let frames = 10 * PAIRED_BLOCK_FRAMES;
        let lines = report_over(
            plan_of(frames, Some(25)),
            Some(arms_over(frames, [10, 30])),
            3,
        );
        assert_eq!(
            lines.len(),
            6,
            "the five clean lines plus the skip: {lines:#?}"
        );
        assert_eq!(
            lines[5],
            "probe: WARNING 3 of 100 frames put nothing new in the reading pane. \
             The frame numbers above include them and are therefore optimistic."
        );

        let lines = report_over(
            plan_of(frames, Some(25)),
            Some(arms_over(frames, [10, 30])),
            0,
        );
        assert_eq!(lines.len(), 5, "and it is absent when no frame was skipped");
    }

    #[test]
    fn the_list_section_appears_only_when_the_list_was_found() {
        let frames = 3 * PAIRED_BLOCK_FRAMES;
        let lines = report_over_with(
            plan_of(frames, None),
            None,
            0,
            Some(ListSection {
                latency: "p50≤1ms p99≤2ms".to_string(),
                realised: 12,
                model_len: 5000,
            }),
            None,
            10,
        );
        assert_eq!(
            lines.len(),
            4,
            "the three unpaired lines plus the list: {lines:#?}"
        );
        assert_eq!(
            lines[3],
            "probe: virtual-list CTK Update systems only p50≤1ms p99≤2ms · realised 12 of 5000"
        );

        let lines = report_over_with(plan_of(frames, None), None, 0, None, None, 10);
        assert_eq!(lines.len(), 3, "and is absent when the world had no list");
    }

    #[test]
    fn the_swap_share_is_withheld_when_no_frame_was_measured() {
        // The `frame_total > 0` guard is the one conditional in the reader
        // section, and this pins both arms. Its false arm *is* defensive:
        // `ProbePlan::parse` rejects zero frames outright, so no command line
        // reaches it, and the fixture gets there by building the plan directly.
        // Worth keeping anyway — the guard stops a division by zero being
        // printed as a percentage of nothing, and the report is a diagnostic
        // tool, which is the last place a `NaN%` should be able to appear if
        // the parser's floor is ever relaxed.
        let mut stats = ReaderStats::default();
        for _ in 0..10 {
            stats.swap.record(std::time::Duration::from_millis(5));
            stats.sanitize.record(std::time::Duration::from_millis(1));
        }
        stats.swaps = 10;

        let lines = report_over_with(
            plan_of(10 * PAIRED_BLOCK_FRAMES, None),
            None,
            0,
            None,
            Some(&stats),
            10,
        );
        assert_eq!(
            lines.len(),
            5,
            "three unpaired lines, body swaps, share: {lines:#?}"
        );
        assert!(lines[3].starts_with("probe: body swaps 10 · whole swap "));
        // Ten swaps of 5 ms against a hundred frames of 10 ms: 50 ms of
        // 1000 ms. Asserted because the share is a ratio of *totals*, and the
        // ratio of means the arithmetic replaced would print 50.0% here.
        assert!(
            lines[4].contains("5.0% of the run's total frame time"),
            "{}",
            lines[4]
        );

        let lines = report_over_with(plan_of(0, None), None, 0, None, Some(&stats), 10);
        assert_eq!(
            lines.len(),
            4,
            "a run that measured no frame prints the swaps and withholds the share: {lines:#?}"
        );
        assert!(lines.iter().all(|line| !line.contains("% of the run")));
    }

    #[test]
    fn the_singular_forms_reach_the_report() {
        // Eleven collected blocks: five pairs, with the eleventh left without a
        // partner inside the plan. That is one dropped slot and — five pairs
        // being an odd count under ABBA — one pair trimmed to restore the order
        // balance, so both counters land on 1 in the same run. The plural arms
        // are covered everywhere else; these are the ones a fixture has to be
        // built for, which is why they were the ones still unreached.
        let frames = 13 * PAIRED_BLOCK_FRAMES;
        let arms = arms_over(frames, [10, 30]);
        let paired = arms.paired();
        assert_eq!((paired.dropped_blocks, paired.unbalanced_pairs), (1, 1));
        let lines = report_over(plan_of(frames, Some(25)), Some(arms), 1);
        assert!(
            lines[3].contains("1 block dropped, 1 pair trimmed to balance the order"),
            "{}",
            lines[3]
        );
        // Index 6: the dropped slot puts a retake notice between the paired
        // summary and the per-pair line, so the skip warning is one lower than
        // in a run that lost nothing.
        assert!(
            lines[6].contains("1 of 130 frames put nothing new"),
            "and the skip warning inflects too: {}",
            lines[6]
        );
    }

    #[test]
    fn the_singular_dearer_form_and_the_all_tied_verdict_reach_the_report() {
        // Both arms at one cost, so every pair ties. A measured equality and an
        // empty run must not read alike, and this is the arm that says so.
        let frames = 10 * PAIRED_BLOCK_FRAMES;
        let lines = report_over(
            plan_of(frames, Some(25)),
            Some(arms_over(frames, [10, 10])),
            0,
        );
        assert!(
            lines[4].contains(
                "0 pairs dearer, 0 cheaper, 4 tied · every pair tied, so there is nothing to test"
            ),
            "{}",
            lines[4]
        );

        // Exactly one pair moved off the tie. The command line cannot drive a
        // run into this shape — `arms_over` is uniform by construction — so the
        // block mean is set directly. Block 3 is the *base* arm of its pair
        // (`arm_of` gives it arm 0), and the printed difference is alternate
        // minus base, so making it cheaper is what makes the pair dearer.
        // Raising it instead reads as "1 cheaper", which is how this fixture
        // was wrong the first time.
        let mut arms = arms_over(frames, [10, 10]);
        for block in &mut arms.blocks {
            if block.index == 3 {
                block.mean_us = 5_000;
            }
        }
        let lines = report_over(plan_of(frames, Some(25)), Some(arms), 0);
        assert!(
            lines[4].contains("1 pair dearer, 0 cheaper, 3 tied"),
            "{}",
            lines[4]
        );
    }

    #[test]
    fn a_run_that_crosses_the_screen_says_so_without_calling_the_figure_a_bound() {
        // Six pairs all one way is p=0.031, the shortest run this probe can
        // produce that crosses 0.05. The wording is the load-bearing half: this
        // is the branch that must not let a reference probability read as a
        // bound, and until now only its p>0.05 sibling was ever delivered.
        let frames = 14 * PAIRED_BLOCK_FRAMES;
        let arms = arms_over(frames, [10, 30]);
        assert_eq!(arms.paired().pairs(), 6);
        let lines = report_over(plan_of(frames, Some(25)), Some(arms), 0);
        assert!(
            lines[4].contains("two-sided sign test p=0.031"),
            "{}",
            lines[4]
        );
        assert!(
            lines[4].contains(
                "it is a reference probability and not a bound; the direction rests on \
                 replication across runs"
            ),
            "{}",
            lines[4]
        );
    }

    #[test]
    fn the_saturation_note_reaches_the_whole_frame_line_and_stays_off_an_empty_run() {
        // Delivery: nothing tested that the whole-frame line appends what
        // `tail_note` returns.
        //
        // A flat 10 ms run does *not* trigger it, which is worth stating
        // because it is the obvious fixture to reach for: `quantile_us` answers
        // with the enclosing bucket's *ceiling* (12 ms) while `max_us` answers
        // with the true maximum (10 ms), so the two disagree and the note
        // correctly stays off. Recording at 2000 ms puts the run in the
        // open-ended top bucket, where `quantile_us` returns the true maximum
        // and the two agree. That is not the only way to make them agree —
        // samples sitting exactly on a closed ceiling do too, which is the
        // benign case `tail_note`'s own documentation describes — but it is the
        // case the note exists for.
        let lines = report_over_with(
            plan_of(10 * PAIRED_BLOCK_FRAMES, None),
            None,
            0,
            None,
            None,
            2_000,
        );
        assert!(
            lines[2].contains("a quantile has landed on max — read the exact mean"),
            "{}",
            lines[2]
        );

        let lines = report_over(plan_of(0, None), None, 0);
        assert!(
            !lines[2].contains("landed on max"),
            "a run that recorded nothing has no tail to flag: {}",
            lines[2]
        );
    }

    #[test]
    fn a_saturated_p99_is_flagged_even_when_the_median_is_well_behaved() {
        // `tail_note`'s own logic, which until now had no test of its own: the
        // delivery test above was its only caller, and that one saturates both
        // quantiles at once, so it would pass just as well against a version
        // that checked p50 alone. This is the shape the function's
        // documentation calls the more dangerous half to miss — a run whose
        // median is honest and whose p99 has landed on the maximum is exactly
        // where p99 is the number a reader trusts.
        let mut histogram = LatencyHistogram::default();
        for _ in 0..98 {
            histogram.record(std::time::Duration::from_millis(16));
        }
        histogram.record(std::time::Duration::from_millis(130));
        histogram.record(std::time::Duration::from_millis(1050));
        // The premise, asserted rather than assumed. Without these two lines
        // the test says only "the note fired", which a p50-only implementation
        // also satisfies the moment a fixture or bucket change makes p50 land
        // on the maximum too — it would keep passing while quietly ceasing to
        // kill the mutation it exists for.
        assert_ne!(
            histogram.quantile_us(0.5),
            histogram.max_us(),
            "the median must be the honest half, or this is not a p99-only fixture"
        );
        assert_eq!(
            histogram.quantile_us(0.99),
            histogram.max_us(),
            "and p99 must be the saturated half"
        );
        assert!(
            !tail_note(&histogram).is_empty(),
            "p99 has landed on the maximum while p50 has not"
        );
    }

    #[test]
    fn an_unpaired_run_reports_without_a_paired_section() {
        // The plan named no alternate stride, so there are no arms and the two
        // paired lines must not appear at all — as distinct from appearing with
        // nothing in them.
        let lines = report_over(plan_of(10 * PAIRED_BLOCK_FRAMES, None), None, 0);
        assert_eq!(lines.len(), 3, "header, repeats, whole frame: {lines:#?}");
        assert!(lines[0].contains("stride 5, offset 0"));
        assert!(
            lines.iter().all(|line| !line.contains("paired")),
            "an unpaired run must not print a paired section"
        );
    }
}
