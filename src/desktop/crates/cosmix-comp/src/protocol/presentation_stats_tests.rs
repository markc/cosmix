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

#[test]
fn percentiles_use_nearest_rank_over_the_newest_samples() {
    let mut ring = Ring::default();
    assert_eq!((ring.percentile(50), ring.max()), (None, None));
    for value in 1..=100 {
        ring.push(value);
    }
    assert_eq!(ring.percentile(50), Some(50));
    assert_eq!(ring.percentile(99), Some(99));
    assert_eq!(ring.max(), Some(100));
    assert_eq!(ring.newest(3), [98, 99, 100]);
    for value in 0..STATS_RING as u64 {
        ring.push(1_000 + value);
    }
    assert_eq!(ring.len(), STATS_RING, "the ring keeps the newest 512");
    assert_eq!(ring.percentile(50), Some(1_000 + 255));
    let mut one = Ring::default();
    one.push(7);
    assert_eq!((one.percentile(50), one.percentile(99)), (Some(7), Some(7)));
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
    stats.mark_input(InputMark {
        input_seq: 1,
        at_us: 1_000,
    });
    // Committed before the input: does not answer it.
    stats.record_present(present(2_000, 900, None));
    assert!(stats.input_to_present_us.newest(4).is_empty());
    stats.record_present(present(5_000, 1_500, None));
    stats.record_present(present(9_000, 6_000, None));
    assert_eq!(stats.input_to_present_us.newest(4), [4_000]);
    assert_eq!(stats.leaves().input_to_present_p50_us, Some(4_000));
}

#[test]
fn reset_zeroes_and_restarts_the_window() {
    let mut stats = PresentationStats::new(0);
    stats.record_present(present(R, 0, Some(R)));
    stats.record_present(present(3 * R, R, Some(R)));
    stats.record_discarded(2);
    stats.reset(99);
    assert_eq!(stats, PresentationStats::new(99));
    assert_eq!(
        stats.leaves(),
        PresentationLeaves {
            since_us: 99,
            ..PresentationLeaves::default()
        }
    );
}

fn frame(registry: &mut StatsRegistry, surfaces: &[(u64, bool, u64, bool)], tv: u64) {
    let mut fold = WindowFrame::default();
    for (surface, is_root, seq, shown) in surfaces {
        registry.surface_frame(*surface, *is_root, *seq, *shown, &mut fold);
    }
    registry.window_frame(1, 7, fold, tv, Some(R));
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
fn window_updates_hidden_by_a_frame_are_discarded_when_a_later_one_shows() {
    let mut registry = StatsRegistry::new(0);
    let window = Some((1, 7));
    registry.note_published(1, window, 1, 0);
    frame(&mut registry, &[(1, true, 1, true)], R);
    // Minimised: 2 and 3 are published but never shown.
    registry.note_published(1, window, 2, R + 1);
    frame(&mut registry, &[(1, true, 2, false)], 2 * R);
    registry.note_published(1, window, 3, 2 * R + 1);
    frame(&mut registry, &[(1, true, 3, false)], 3 * R);
    // Restored with 4, long after the hidden commits: no miss, no interval,
    // and the untimeable hidden commits do not count as pending.
    registry.note_published(1, window, 4, 99 * R);
    frame(&mut registry, &[(1, true, 4, true)], 100 * R);
    let stats = registry.window(1, 7).unwrap();
    assert_eq!((stats.presented, stats.discarded), (2, 2));
    assert_eq!(stats.missed, Some(0));
    assert!(stats.intervals_us.newest(4).is_empty());
    assert_eq!(stats.commit_to_present_us.newest(4), [R, R]);
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

    frame(&mut registry, &[(1, true, 1, true)], 2 * R);
    registry.reset_window(1, 7, 600);
    assert_eq!(registry.window(1, 7).unwrap().since_us, 600);
}

#[test]
fn input_marks_reach_the_window_and_the_source_lookup() {
    let mut registry = StatsRegistry::new(0);
    registry.mark_input(Some((1, 7)), 42, 1_000);
    registry.mark_input(None, 43, 2_000);
    assert_eq!(registry.input_mark(42).map(|mark| mark.at_us), Some(1_000));
    assert_eq!(registry.input_mark(43).map(|mark| mark.at_us), Some(2_000));
    assert_eq!(registry.input_mark(44), None);
    registry.note_published(1, Some((1, 7)), 1, 1_500);
    frame(&mut registry, &[(1, true, 1, true)], 5_000);
    assert_eq!(
        registry.window(1, 7).unwrap().input_to_present_us.newest(4),
        [4_000]
    );
    for seq in 0..INPUT_MARKS as u64 {
        registry.mark_input(None, 100 + seq, 0);
    }
    assert_eq!(registry.input_mark(42), None, "old marks age out");
}
