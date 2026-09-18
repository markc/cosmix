use super::*;

const R: u64 = 16_667;

fn present(tv_us: u64, committed: u64, refresh: Option<u64>) -> PresentSample {
    PresentSample {
        tv_us,
        refresh_us: refresh,
        committed_us: Some(committed),
        pending_since_us: Some(committed),
        ..PresentSample::default()
    }
}

fn mark(input_seq: u64, injected_at_us: u64) -> InputMark {
    InputMark {
        input_seq,
        injected_at_us,
    }
}

#[test]
fn percentiles_use_nearest_rank_over_the_newest_samples() {
    let mut ring = Ring::default();
    assert_eq!(ring.summary(), RingSummary::default());
    for value in 1..=100 {
        ring.push(value);
    }
    assert_eq!(
        ring.summary(),
        RingSummary {
            p50: Some(50),
            p99: Some(99),
            max: Some(100),
        }
    );
    assert_eq!(ring.newest(3), [98, 99, 100]);
    for value in 0..STATS_RING as u64 {
        ring.push(1_000 + value);
    }
    assert_eq!(ring.len(), STATS_RING, "the ring keeps the newest 512");
    assert_eq!(ring.summary().p50, Some(1_000 + 255));
    let mut one = Ring::default();
    one.push(7);
    assert_eq!(
        one.summary(),
        RingSummary {
            p50: Some(7),
            p99: Some(7),
            max: Some(7),
        }
    );
}

#[test]
fn a_two_vblank_gap_with_a_pending_commit_is_one_miss() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    assert_eq!(stats.missed, Some(0));
    // Committed right after the previous frame, shown one vblank late.
    stats.record_present(present(3 * R, R + 100, Some(R)));
    assert_eq!(stats.missed, Some(1));
    assert_eq!(stats.intervals_us.newest(4), [2 * R]);
    assert_eq!(stats.commit_to_present_us.newest(4), [R, 2 * R - 100]);
}

#[test]
fn an_idle_gap_is_not_a_miss() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    // Nothing was committed until just before the vblank that showed it.
    stats.record_present(present(5 * R, 4 * R + 100, Some(R)));
    assert_eq!(stats.missed, Some(0));
    assert_eq!(stats.leaves().interval_max_us, Some(4 * R));
}

#[test]
fn unknown_refresh_leaves_missed_unmeasured() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, None));
    stats.record_present(present(5 * R, R, None));
    assert_eq!(stats.missed, None);
    assert_eq!(stats.leaves().missed, None);
    assert_eq!(stats.leaves().refresh_us, None);
    #[cfg(feature = "bus")]
    assert_eq!(stats.leaves().to_json()["missed"], Value::Null);
}

/// S4: a known count goes back to null when the refresh becomes unknown.
#[test]
fn missed_returns_to_null_when_the_refresh_becomes_unknown() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    stats.record_present(present(3 * R, R + 1, Some(R)));
    assert_eq!(stats.missed, Some(1));
    stats.record_present(present(4 * R, 3 * R + 1, None));
    assert_eq!((stats.missed, stats.refresh_us), (None, None));
}

#[test]
fn a_hidden_gap_is_neither_an_interval_nor_a_miss() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    stats.hidden();
    stats.record_present(present(100 * R, 0, Some(R)));
    assert_eq!(stats.missed, Some(0));
    assert!(stats.intervals_us.newest(4).is_empty());
}

#[test]
fn input_to_present_takes_the_first_update_committed_after_the_mark() {
    let mut stats = PresentationStats::new(0);
    stats.mark_input(mark(1, 1_000));
    // Committed before the input: does not answer it.
    stats.record_present(present(2_000, 900, None));
    assert!(stats.input_to_present_us.newest(4).is_empty());
    stats.record_present(present(5_000, 1_500, None));
    stats.record_present(present(9_000, 6_000, None));
    assert_eq!(stats.input_to_present_us.newest(4), [4_000]);
    assert_eq!(stats.leaves().input_to_present_p50_us, Some(4_000));
}

/// S2: a mark expires after a second and on a hide or reset.
#[test]
fn input_marks_expire() {
    let mut stats = PresentationStats::new(0);
    stats.mark_input(mark(1, 1_000));
    // The age that matters is the client's (round 2): a mark expires when
    // the answering COMMIT is more than a TTL after the input, so a late
    // commit misses it while a slow present of a prompt commit would not.
    let late_commit = 1_000 + INPUT_MARK_TTL_US + 1;
    stats.record_present(present(late_commit + 500, late_commit, None));
    assert!(stats.input_to_present_us.newest(4).is_empty(), "too late");
    stats.record_present(present(3_000_000, 2_900_000, None));
    assert!(stats.input_to_present_us.newest(4).is_empty(), "and gone");

    stats.mark_input(mark(2, 4_000_000));
    stats.hidden();
    stats.record_present(present(4_000_100, 4_000_050, None));
    assert!(
        stats.input_to_present_us.newest(4).is_empty(),
        "dropped on hide"
    );

    stats.mark_input(mark(3, 5_000_000));
    stats.reset(5_000_001);
    stats.record_present(present(5_000_100, 5_000_050, None));
    assert!(
        stats.input_to_present_us.newest(4).is_empty(),
        "dropped on reset"
    );
}

#[test]
fn reset_zeroes_and_restarts_the_window() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    stats.record_present(present(3 * R, R, Some(R)));
    stats.record_discarded(2, None);
    stats.reset(99);
    assert_eq!(stats, PresentationStats::new(99));
    assert_eq!(
        stats.leaves(),
        PresentationLeaves {
            since_us: 99,
            ..PresentationLeaves::default()
        }
    );
    // S5: a frame shown before the reset but reported after it is ignored.
    // Compared through the observable leaves: the drop is logged once, and
    // that private flag is not part of what a reader can see.
    stats.record_present(present(50, 10, Some(R)));
    assert_eq!(
        stats.leaves(),
        PresentationLeaves {
            since_us: 99,
            ..PresentationLeaves::default()
        }
    );
    assert_eq!(stats.clock_base_mismatches, 0);
}

/// S7: counts saturate instead of overflowing.
#[test]
fn counts_saturate() {
    let mut stats = PresentationStats::new(0);
    stats.record_discarded(u64::MAX - 1, None);
    stats.record_present(PresentSample {
        tv_us: 1,
        discarded: 5,
        ..PresentSample::default()
    });
    stats.record_discarded(5, None);
    assert_eq!(stats.discarded, u64::MAX);
}

fn frame_with(registry: &mut StatsRegistry, surfaces: &[(u64, bool, u64, SurfaceShown)], tv: u64) {
    let mut fold = WindowFrame::default();
    for (surface, is_root, seq, state) in surfaces {
        registry.surface_frame(*surface, *is_root, *seq, *state, &mut fold);
    }
    registry.window_frame(1, 7, fold, tv, Some(R));
}

fn frame(registry: &mut StatsRegistry, surfaces: &[(u64, bool, u64, bool)], tv: u64) {
    let states = surfaces
        .iter()
        .map(|(surface, is_root, seq, shown)| {
            let state = if *shown {
                SurfaceShown::Shown
            } else {
                SurfaceShown::Hidden
            };
            (*surface, *is_root, *seq, state)
        })
        .collect::<Vec<_>>();
    frame_with(registry, &states, tv);
}

#[test]
fn window_stats_follow_content_and_roll_subsurfaces_up() {
    let mut registry = StatsRegistry::new(5);
    let window = Some((1, 7));
    registry.note_published(1, window, 1, 10);
    registry.note_published(2, window, 1, 12);
    frame(&mut registry, &[(1, true, 1, true), (2, false, 1, true)], R);
    let stats = registry.window(1, 7).unwrap();
    // Two surfaces updated in one frame: one presentation of the window.
    assert_eq!((stats.presented, stats.discarded), (1, 0));
    assert_eq!(stats.commit_to_present_us.newest(4), [R - 10]);
    assert_eq!(stats.since_us, 10);

    // The subsurface published 2 and 3 before the next frame showed 3.
    registry.note_published(2, window, 2, R + 10);
    registry.note_published(2, window, 3, R + 20);
    frame(
        &mut registry,
        &[(1, true, 1, true), (2, false, 3, true)],
        2 * R,
    );
    let stats = registry.window(1, 7).unwrap();
    assert_eq!((stats.presented, stats.discarded), (2, 1));
    assert_eq!(stats.commit_to_present_us.newest(4), [R - 10, R - 20]);
    assert_eq!(stats.intervals_us.newest(4), [R]);

    // Nothing new: nothing recorded.
    frame(
        &mut registry,
        &[(1, true, 1, true), (2, false, 3, true)],
        3 * R,
    );
    assert_eq!(registry.window(1, 7).unwrap().presented, 2);

    // A stale generation reads as absent; a new one starts over.
    assert!(registry.window(1, 6).is_none());
    registry.note_published(1, Some((1, 8)), 2, 4 * R);
    assert_eq!(registry.window(1, 8).unwrap().presented, 0);
    assert!(registry.window(1, 7).is_none());

    registry.forget_surface(1);
    assert!(registry.window(1, 8).is_none());
}

#[test]
fn window_updates_hidden_by_a_frame_are_discarded_then() {
    let mut registry = StatsRegistry::new(0);
    let window = Some((1, 7));
    registry.note_published(1, window, 1, 0);
    frame(&mut registry, &[(1, true, 1, true)], R);
    // Minimised: 2 and 3 are published but never shown; each hidden frame
    // discards what it did not show, as the feedback ledger does.
    registry.note_published(1, window, 2, R + 1);
    frame(&mut registry, &[(1, true, 2, false)], 2 * R);
    assert_eq!(registry.window(1, 7).unwrap().discarded, 1);
    registry.note_published(1, window, 3, 2 * R + 1);
    frame(&mut registry, &[(1, true, 3, false)], 3 * R);
    // S8: restored showing 3 again, which the ledger discarded: not a
    // presentation.
    frame(&mut registry, &[(1, true, 3, true)], 4 * R);
    let stats = registry.window(1, 7).unwrap();
    assert_eq!((stats.presented, stats.discarded), (1, 2));
    // New content 4, long after: no miss, no interval across the hide.
    registry.note_published(1, window, 4, 99 * R);
    frame(&mut registry, &[(1, true, 4, true)], 100 * R);
    let stats = registry.window(1, 7).unwrap();
    assert_eq!((stats.presented, stats.discarded), (2, 2));
    assert_eq!(stats.missed, Some(0));
    assert!(stats.intervals_us.newest(4).is_empty());
    assert_eq!(stats.commit_to_present_us.newest(4), [R, R]);
}

/// S3: a visible surface whose newest content is not sampled yet is a
/// stall: the run continues and the eventual presentation counts the
/// skipped vblanks.
#[test]
fn a_stalled_upload_is_a_miss_not_a_hide() {
    let mut registry = StatsRegistry::new(0);
    let window = Some((1, 7));
    registry.note_published(1, window, 1, 0);
    frame(&mut registry, &[(1, true, 1, true)], R);
    registry.note_published(1, window, 2, R + 10);
    // Two frames while seq 2 is still uploading; they report the last
    // sampled seq.
    frame_with(&mut registry, &[(1, true, 1, SurfaceShown::Waiting)], 2 * R);
    frame_with(&mut registry, &[(1, true, 1, SurfaceShown::Waiting)], 3 * R);
    frame(&mut registry, &[(1, true, 2, true)], 4 * R);
    let stats = registry.window(1, 7).unwrap();
    assert_eq!((stats.presented, stats.discarded), (2, 0));
    assert_eq!(stats.intervals_us.newest(4), [3 * R]);
    assert_eq!(stats.missed, Some(2));
    assert_eq!(stats.commit_to_present_us.newest(4), [R, 3 * R - 10]);
}

/// B-N1: a frame reported after a reset drops its discards too, not just
/// its presentation; B-N3 bounds that and counts a frame from another
/// clock base instead of freezing the row.
#[test]
fn a_frame_older_than_the_reset_is_dropped_whole_unless_its_clock_differs() {
    // The reset sits above the pre-reset bound so "far older" is reachable
    // without underflowing (the bound is 10 s; a 1 s base cannot express it).
    const RESET_US: u64 = 2 * PRE_RESET_BOUND_US;
    let mut stats = PresentationStats::new(RESET_US);
    stats.record_discarded(3, Some(RESET_US - 1_000));
    assert_eq!((stats.presented, stats.discarded), (0, 0));
    stats.record_present(PresentSample {
        tv_us: RESET_US - 1_000,
        discarded: 2,
        ..PresentSample::default()
    });
    assert_eq!((stats.presented, stats.discarded), (0, 0));
    assert_eq!(stats.clock_base_mismatches, 0);
    // Far older than the reset: a different clock base, counted and flagged.
    stats.record_present(PresentSample {
        tv_us: RESET_US - PRE_RESET_BOUND_US - 1,
        discarded: 1,
        ..PresentSample::default()
    });
    assert_eq!((stats.presented, stats.discarded), (1, 1));
    assert_eq!(stats.clock_base_mismatches, 1);
    stats.record_discarded(1, Some(0));
    assert_eq!(stats.clock_base_mismatches, 2);
}

/// A-N2: a window the report does not list keeps an injected input's mark
/// (the mark's own age bounds it); a genuine hide drops it.
#[test]
fn an_unlisted_frame_keeps_the_input_mark() {
    let mut registry = StatsRegistry::new(0);
    registry.mark_input(Some((1, 7)), mark(1, 1_000));
    registry.hide_unlisted(|_| false);
    registry.note_published(1, Some((1, 7)), 1, 2_000);
    frame(&mut registry, &[(1, true, 1, true)], 5_000);
    assert_eq!(
        registry.window(1, 7).unwrap().input_to_present_us.newest(4),
        [4_000],
        "an unlisted frame is not a hide"
    );
    registry.mark_input(Some((1, 7)), mark(2, 6_000));
    frame(&mut registry, &[(1, true, 1, false)], 7_000);
    registry.note_published(1, Some((1, 7)), 2, 8_000);
    frame(&mut registry, &[(1, true, 2, true)], 9_000);
    assert_eq!(
        registry.window(1, 7).unwrap().input_to_present_us.newest(4),
        [4_000],
        "a hide drops the mark"
    );
}

/// A-N3: the mark ages against the client's commit, not a slow present.
#[test]
fn a_slow_present_does_not_lose_a_promptly_answered_input() {
    let mut stats = PresentationStats::new(0);
    stats.mark_input(mark(1, 1_000));
    stats.record_present(present(2_000 + INPUT_MARK_TTL_US, 1_500, None));
    assert_eq!(
        stats.input_to_present_us.newest(4),
        [1_000 + INPUT_MARK_TTL_US]
    );
}

/// S1: a window the report does not list is not shown by that frame.
#[test]
fn unlisted_windows_lose_their_run() {
    let mut registry = StatsRegistry::new(0);
    registry.note_published(1, Some((1, 7)), 1, 0);
    frame(&mut registry, &[(1, true, 1, true)], R);
    registry.hide_unlisted(|_| false);
    assert_eq!(registry.last_input_seq, 0, "nothing marked yet");
    registry.note_published(1, Some((1, 7)), 2, 5 * R);
    frame(&mut registry, &[(1, true, 2, true)], 6 * R);
    let stats = registry.window(1, 7).unwrap();
    assert!(stats.intervals_us.newest(4).is_empty());
    assert_eq!(stats.missed, Some(0));
    registry.hide_unlisted(|window| window == 1);
    frame(&mut registry, &[(1, true, 2, true)], 7 * R);
    registry.note_published(1, Some((1, 7)), 3, 7 * R + 1);
    frame(&mut registry, &[(1, true, 3, true)], 8 * R);
    assert_eq!(
        registry.window(1, 7).unwrap().intervals_us.newest(4),
        [2 * R]
    );
}

#[test]
fn output_stats_count_frames_and_reset_restarts_everything() {
    let mut registry = StatsRegistry::new(3);
    registry.output_frame("DP-1", 100, 0x7, Some(R));
    registry.output_frame("DP-1", 100 + R, 0x7, Some(R));
    let output = registry.output("DP-1").unwrap();
    assert_eq!((output.frames, output.since_us), (2, 3));
    assert_eq!(output.intervals_us.newest(4), [R]);
    assert_eq!((output.flags, output.refresh_us), (0x7, Some(R)));

    registry.note_published(1, Some((1, 7)), 1, 0);
    frame(&mut registry, &[(1, true, 1, true)], R);
    registry.reset_all(500);
    assert_eq!(registry.epoch_us, 500);
    assert_eq!(registry.output("DP-1").unwrap().frames, 0);
    assert_eq!(registry.output("DP-1").unwrap().since_us, 500);
    assert_eq!(registry.window(1, 7).unwrap().presented, 0);
    assert_eq!(registry.window(1, 7).unwrap().since_us, 500);
    // A frame from before the reset is not counted.
    registry.output_frame("DP-1", 400, 0x7, Some(R));
    assert_eq!(registry.output("DP-1").unwrap().frames, 0);

    frame(&mut registry, &[(1, true, 1, true)], 2 * R);
    registry.reset_window(1, 7, 600);
    assert_eq!(registry.window(1, 7).unwrap().since_us, 600);
}

/// S5: an update published before a reset is not timed against it.
#[test]
fn reset_drops_pending_publish_times() {
    let mut registry = StatsRegistry::new(0);
    registry.note_published(1, Some((1, 7)), 1, 100);
    registry.reset_window(1, 7, 10_000);
    frame(&mut registry, &[(1, true, 1, true)], 20_000);
    let stats = registry.window(1, 7).unwrap();
    assert_eq!(stats.presented, 1);
    assert!(stats.commit_to_present_us.newest(4).is_empty());

    registry.note_published(1, Some((1, 7)), 2, 30_000);
    registry.reset_all(40_000);
    frame(&mut registry, &[(1, true, 2, true)], 50_000);
    assert!(
        registry
            .window(1, 7)
            .unwrap()
            .commit_to_present_us
            .newest(4)
            .is_empty()
    );
}

#[test]
fn input_marks_reach_the_window_and_the_source_lookup() {
    let mut registry = StatsRegistry::new(0);
    assert!(registry.mark_input(Some((1, 7)), mark(42, 1_000)));
    assert!(registry.mark_input(None, mark(43, 2_000)));
    // S10: an input_seq names one injection.
    assert!(!registry.mark_input(None, mark(43, 2_500)));
    assert!(!registry.mark_input(None, mark(10, 2_500)));
    let at = |registry: &StatsRegistry, seq| {
        registry
            .input_mark(seq, 3_000)
            .map(|mark| mark.injected_at_us)
    };
    assert_eq!(at(&registry, 42), Some(1_000));
    assert_eq!(at(&registry, 43), Some(2_000));
    assert_eq!(at(&registry, 44), None);
    assert_eq!(
        registry.input_mark(42, 1_000 + INPUT_MARK_TTL_US + 1),
        None,
        "S2: expired"
    );
    registry.note_published(1, Some((1, 7)), 1, 1_500);
    frame(&mut registry, &[(1, true, 1, true)], 5_000);
    assert_eq!(
        registry.window(1, 7).unwrap().input_to_present_us.newest(4),
        [4_000]
    );
    for seq in 0..INPUT_MARKS as u64 {
        registry.mark_input(None, mark(100 + seq, 3_000));
    }
    assert_eq!(at(&registry, 42), None, "old marks age out");
    // Marks older than the TTL are pruned as new ones arrive.
    registry.mark_input(None, mark(1_000, 3_000 + INPUT_MARK_TTL_US + 1));
    assert_eq!(registry.input_marks.len(), 1);
    // A-N1: a reset forgets the sequence watermark; the injection site's
    // own counter is what keeps sequences unique.
    registry.reset_all(4_000);
    assert_eq!(registry.last_input_seq, 0);
    assert!(registry.mark_input(None, mark(1, 4_100)));
}
