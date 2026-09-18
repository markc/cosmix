use super::*;
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolved {
    Presented(u32, u64),
    Discarded(u32),
}

type Log = Rc<RefCell<Vec<Resolved>>>;

/// A fake feedback that records its resolution and panics when dropped
/// unresolved (ownership already makes a second resolution impossible).
struct Fake {
    tag: u32,
    log: Log,
    resolved: bool,
}

impl Fake {
    fn new(tag: u32, log: &Log) -> Self {
        Self {
            tag,
            log: Rc::clone(log),
            resolved: false,
        }
    }
}

impl Feedback for Fake {
    fn presented(mut self, frame: &PresentedFrame) -> bool {
        self.resolved = true;
        if frame.output.is_none() && frame.seq == u64::MAX {
            // Test hook: behave like a frame with no nameable output.
            self.log.borrow_mut().push(Resolved::Discarded(self.tag));
            return false;
        }
        self.log
            .borrow_mut()
            .push(Resolved::Presented(self.tag, frame.time.as_micros() as u64));
        true
    }

    fn discarded(mut self) {
        self.resolved = true;
        self.log.borrow_mut().push(Resolved::Discarded(self.tag));
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if !self.resolved && !std::thread::panicking() {
            panic!("feedback {} dropped unresolved", self.tag);
        }
    }
}

fn frame(time_us: u64) -> PresentedFrame {
    PresentedFrame {
        output: None,
        time: Duration::from_micros(time_us),
        refresh: Refresh::Unknown,
        seq: 0,
        flags: wp_presentation_feedback::Kind::empty(),
    }
}

const S: SurfaceId = SurfaceId(7);

#[test]
fn only_the_shown_commit_is_presented_and_older_ones_were_superseded() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    for (tag, seq) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)] {
        ledger.take_on_commit(S, seq, vec![Fake::new(tag, &log)]);
    }
    let resolution = ledger.resolve(S, 2, true, &frame(100));
    assert_eq!(
        resolution,
        Resolution {
            presented: Some((2, 2)),
            discarded: vec![(1, 1)],
        }
    );
    assert_eq!(
        *log.borrow(),
        [
            Resolved::Discarded(1),
            Resolved::Presented(2, 100),
            Resolved::Presented(3, 100),
        ]
    );
    assert_eq!(ledger.pending_count(S), 2, "commits above the frame wait");
    assert_eq!(
        ledger.resolve(S, 2, true, &frame(200)),
        Resolution::default(),
        "never twice"
    );
    // A frame that shows commit 4 never showed commit 3: superseded.
    let resolution = ledger.resolve(S, 4, true, &frame(300));
    assert_eq!(resolution.presented, Some((4, 1)));
    assert_eq!(resolution.discarded, [(3, 1)]);
    assert_eq!(
        log.borrow()[3..],
        [Resolved::Discarded(4), Resolved::Presented(5, 300)]
    );
    assert_eq!(
        ledger.counters(S),
        PresentationCounters {
            presented: 3,
            discarded: 2,
            last_presented_us: Some(300),
        }
    );
}

#[test]
fn a_frame_past_every_pending_commit_presents_nothing() {
    // The frame sampled commit 9, which has no feedback: commits 1..=3 were
    // all superseded before any frame showed them.
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    for seq in 1..=3 {
        ledger.take_on_commit(S, seq, vec![Fake::new(seq as u32, &log)]);
    }
    let resolution = ledger.resolve(S, 9, true, &frame(1));
    assert_eq!(resolution.presented, None);
    assert_eq!(resolution.discarded, [(1, 1), (2, 1), (3, 1)]);
    assert_eq!(ledger.counters(S).presented, 0);
}

#[test]
fn not_shown_discards_everything_up_to_the_frame() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log)]);
    ledger.take_on_commit(S, 2, vec![Fake::new(2, &log)]);
    ledger.take_on_commit(S, 3, vec![Fake::new(3, &log)]);
    let resolution = ledger.resolve(S, 2, false, &frame(1));
    assert_eq!(resolution.presented, None);
    assert_eq!(resolution.discarded, [(1, 1), (2, 1)]);
    assert_eq!(
        *log.borrow(),
        [Resolved::Discarded(1), Resolved::Discarded(2)]
    );
    assert_eq!(ledger.pending_count(S), 1);
    assert_eq!(ledger.discard_surface(S).discarded, [(3, 1)]);
    assert_eq!(log.borrow().last(), Some(&Resolved::Discarded(3)));
    assert!(ledger.pending_surfaces().is_empty());
}

#[test]
fn a_frame_without_an_output_counts_as_discarded() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log), Fake::new(2, &log)]);
    let no_output = PresentedFrame {
        seq: u64::MAX,
        ..frame(5)
    };
    let resolution = ledger.resolve(S, 1, true, &no_output);
    assert_eq!(resolution.presented, None);
    assert_eq!(resolution.discarded, [(1, 2)]);
    assert_eq!(
        ledger.counters(S),
        PresentationCounters {
            presented: 0,
            discarded: 2,
            last_presented_us: None,
        }
    );
}

#[test]
fn a_refused_commit_is_discarded_and_others_keep_waiting() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log)]);
    ledger.take_on_commit(S, 2, vec![Fake::new(2, &log)]);
    // A bufferless commit after 2 inherits its sequence.
    ledger.take_on_commit(S, 2, vec![Fake::new(3, &log)]);
    ledger.take_on_commit(S, 3, vec![Fake::new(4, &log)]);
    assert_eq!(ledger.discard_range(S, 2, 2).discarded, [(2, 2)]);
    assert_eq!(
        *log.borrow(),
        [Resolved::Discarded(2), Resolved::Discarded(3)]
    );
    assert_eq!(ledger.pending_count(S), 2);
    assert_eq!(ledger.discard_range(S, 9, 9), Resolution::default());
    // A range takes superseded requests too, and nothing outside it.
    ledger.take_on_commit(S, 4, vec![Fake::new(5, &log)]);
    ledger.take_on_commit(S, 5, vec![Fake::new(6, &log)]);
    assert_eq!(ledger.discard_range(S, 2, 4).discarded, [(3, 1), (4, 1)]);
    assert_eq!(ledger.pending_count(S), 2, "seq 1 and 5 remain");
    ledger.discard_surface(S);
}

#[test]
fn a_fast_client_loses_its_oldest_commits_not_memory() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    let mut overflow = Vec::new();
    for seq in 1..=(MAX_PENDING_COMMITS as u64 + 3) {
        overflow.extend(
            ledger
                .take_on_commit(S, seq, vec![Fake::new(seq as u32, &log)])
                .discarded,
        );
    }
    assert_eq!(overflow, [(1, 1), (2, 1), (3, 1)]);
    assert_eq!(ledger.pending_count(S), MAX_PENDING_COMMITS);
    assert_eq!(
        *log.borrow(),
        [
            Resolved::Discarded(1),
            Resolved::Discarded(2),
            Resolved::Discarded(3)
        ]
    );
    // Dropping the ledger resolves the rest (the fake would panic otherwise).
    drop(ledger);
    assert_eq!(log.borrow().len(), MAX_PENDING_COMMITS + 3);
}

#[test]
fn surfaces_are_independent_and_forgetting_one_drops_its_counters() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    let other = SurfaceId(8);
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log)]);
    ledger.take_on_commit(other, 1, vec![Fake::new(2, &log)]);
    assert_eq!(
        ledger.resolve(other, 1, true, &frame(5)).presented,
        Some((1, 1))
    );
    assert_eq!(ledger.pending_count(S), 1);
    ledger.discard_surface(S);
    ledger.forget_counters(S);
    assert_eq!(ledger.counters(S), PresentationCounters::default());
    assert_eq!(
        *log.borrow(),
        [Resolved::Presented(2, 5), Resolved::Discarded(1)]
    );
}

fn source(revision: u64, shown: bool, upload: u64) -> FrameSource {
    FrameSource {
        id: "scene".into(),
        revision,
        shown,
        upload_bytes: upload,
        damage_px: upload / 2,
        consumed_input: None,
        revised_us: None,
        first_revised_us: None,
    }
}

fn no_marks(_: u64, _: u64) -> Option<InputMark> {
    None
}

fn presented_discarded(counters: &SourceCounters) -> (u64, u64) {
    (counters.stats.presented, counters.stats.discarded)
}

#[test]
fn source_revisions_skipped_between_frames_count_as_discarded() {
    let mut ledger = SourceLedger::default();
    assert_eq!(ledger.register("scene", Some("DP-1".into()), 1), 1);
    ledger.resolve(&source(1, true, 10), 1, None, no_marks);
    ledger.resolve(&source(1, true, 0), 2, None, no_marks);
    ledger.resolve(&source(4, true, 30), 3, None, no_marks);
    let counters = ledger.get("scene").unwrap();
    // Revisions 1 -> 4 between two shown frames: 1 presented + 2 discarded.
    assert_eq!(presented_discarded(counters), (2, 2));
    assert_eq!(counters.stats.last_presented_us, Some(3));
    assert_eq!(counters.upload_bytes_total, 40);
    assert_eq!(counters.damage_px_total, 20);
    assert_eq!(counters.frames, 3);
    assert_eq!(counters.upload_bytes.newest(8), [10, 0, 30]);
    assert_eq!(counters.leaves().upload_bytes_p50, Some(10));
    assert_eq!(
        (counters.output.as_deref(), counters.registered_at_us),
        (Some("DP-1"), 1)
    );
    assert_eq!(
        counters.leaves().common.missed,
        None,
        "unknown refresh is unmeasured, not zero"
    );

    // Hidden updates are not presented; a later shown one supersedes them.
    ledger.resolve(&source(5, false, 1), 4, None, no_marks);
    ledger.resolve(&source(7, true, 1), 5, None, no_marks);
    let counters = ledger.get("scene").unwrap();
    assert_eq!(presented_discarded(counters), (3, 4));
    assert_eq!(
        counters.stats.intervals_us.newest(8),
        [2],
        "the hidden frame broke the run"
    );

    // Revisions never shown before unregistering are discarded, including
    // ones from frames that were never reported (the plugin wrote 11).
    ledger.resolve(&source(9, false, 0), 6, None, no_marks);
    let gone = ledger.unregister("scene", 11).unwrap();
    assert_eq!(presented_discarded(&gone), (3, 8));
    assert!(ledger.get("scene").is_none());
    assert_eq!(ledger.len(), 0, "nothing is kept for a gone source");
    assert_eq!(
        ledger.register("scene", None, 6),
        2,
        "registration numbers never repeat"
    );
    assert_eq!(ledger.register("other", None, 6), 3);
    ledger.resolve(&source(1, true, 0), 7, None, no_marks);
    assert_eq!(ledger.get("scene").unwrap().stats.presented, 1);
}

#[test]
fn source_timing_input_and_reset() {
    let mut ledger = SourceLedger::default();
    ledger.register("scene", None, 0);
    let stamped = |revision, shown, first, newest, input| FrameSource {
        revised_us: Some(newest),
        first_revised_us: Some(first),
        consumed_input: input,
        ..source(revision, shown, 8)
    };
    let marks = |seq: u64, at_us: u64| {
        (seq == 42)
            .then_some(InputMark {
                input_seq: 42,
                injected_at_us: 1_000,
            })
            .filter(|mark| mark.live_at(at_us))
    };
    // 60 Hz: a revision written at 1_000 and shown at 16_000.
    ledger.resolve(
        &stamped(1, true, 1_000, 1_000, None),
        16_000,
        Some(16_000),
        marks,
    );
    // Revision 2 written at 20_000 but shown two vblanks late (64_000):
    // the vblanks at 32_000 and 48_000 had it pending.
    ledger.resolve(
        &stamped(2, true, 20_000, 20_000, Some(42)),
        64_000,
        Some(16_000),
        marks,
    );
    let counters = ledger.get("scene").unwrap();
    assert_eq!(counters.stats.missed, Some(2));
    assert_eq!(
        counters.stats.commit_to_present_us.newest(8),
        [15_000, 44_000]
    );
    assert_eq!(counters.stats.input_to_present_us.newest(8), [63_000]);
    assert_eq!(counters.leaves().common.refresh_us, Some(16_000));
    assert_eq!(counters.upload_bytes.newest(1), [8]);
    assert_eq!(counters.stats.intervals_us.newest(1), [48_000]);
    #[cfg(feature = "bus")]
    {
        let samples = counters.samples(1);
        assert_eq!(samples["upload_bytes"], serde_json::json!([8]));
        assert_eq!(samples["intervals_us"], serde_json::json!([48_000]));
    }

    assert!(ledger.reset("scene", 70_000));
    let counters = ledger.get("scene").unwrap();
    assert_eq!(presented_discarded(counters), (0, 0));
    assert_eq!(
        (
            counters.upload_bytes_total,
            counters.frames,
            counters.stats.since_us
        ),
        (0, 0, 70_000)
    );
    assert_eq!(counters.registration, 1, "a reset keeps the registration");
    // A reset does not re-present the revision already shown.
    ledger.resolve(&stamped(2, true, 20_000, 20_000, None), 80_000, None, marks);
    assert_eq!(ledger.get("scene").unwrap().stats.presented, 0);
    assert!(!ledger.reset("gone", 0));
}

#[test]
fn unregistered_sources_are_ignored() {
    let mut ledger = SourceLedger::default();
    ledger.resolve(&source(1, true, 10), 0, None, no_marks);
    assert!(ledger.get("scene").is_none());
    assert!(ledger.unregister("scene", 1).is_none());
}
