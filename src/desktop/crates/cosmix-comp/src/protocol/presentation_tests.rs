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
    fn presented(mut self, frame: &PresentedFrame) {
        self.resolved = true;
        self.log
            .borrow_mut()
            .push(Resolved::Presented(self.tag, frame.time.as_micros() as u64));
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
fn newest_resolved_commit_is_presented_and_older_ones_superseded() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    for (tag, seq) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 4)] {
        ledger.take_on_commit(S, seq, vec![Fake::new(tag, &log)]);
    }
    assert_eq!(ledger.resolve(S, 2, true, &frame(100)), 2);
    assert_eq!(
        *log.borrow(),
        [
            Resolved::Discarded(1),
            Resolved::Presented(2, 100),
            Resolved::Presented(3, 100),
        ]
    );
    assert_eq!(ledger.pending_count(S), 2, "commits above the frame wait");
    assert_eq!(ledger.resolve(S, 2, true, &frame(200)), 0, "never twice");
    assert_eq!(ledger.resolve(S, 9, true, &frame(300)), 1);
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
fn not_shown_discards_everything_up_to_the_frame() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log)]);
    ledger.take_on_commit(S, 2, vec![Fake::new(2, &log)]);
    ledger.take_on_commit(S, 3, vec![Fake::new(3, &log)]);
    assert_eq!(ledger.resolve(S, 2, false, &frame(1)), 0);
    assert_eq!(
        *log.borrow(),
        [Resolved::Discarded(1), Resolved::Discarded(2)]
    );
    assert_eq!(ledger.pending_count(S), 1);
    assert_eq!(ledger.discard_surface(S), 1);
    assert_eq!(log.borrow().last(), Some(&Resolved::Discarded(3)));
    assert!(ledger.pending_surfaces().is_empty());
}

#[test]
fn a_fast_client_loses_its_oldest_commits_not_memory() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    for seq in 1..=(MAX_PENDING_COMMITS as u64 + 3) {
        ledger.take_on_commit(S, seq, vec![Fake::new(seq as u32, &log)]);
    }
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
fn surfaces_are_independent_and_forgetting_one_resolves_it() {
    let log = Log::default();
    let mut ledger = PresentationLedger::default();
    let other = SurfaceId(8);
    ledger.take_on_commit(S, 1, vec![Fake::new(1, &log)]);
    ledger.take_on_commit(other, 1, vec![Fake::new(2, &log)]);
    assert_eq!(ledger.resolve(other, 1, true, &frame(5)), 1);
    assert_eq!(ledger.pending_count(S), 1);
    ledger.forget_surface(S);
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
    }
}

#[test]
fn source_revisions_skipped_between_frames_count_as_discarded() {
    let mut ledger = SourceLedger::default();
    assert_eq!(ledger.register("scene"), 1);
    ledger.resolve(&source(1, true, 10), Duration::from_micros(1));
    ledger.resolve(&source(1, true, 0), Duration::from_micros(2));
    ledger.resolve(&source(4, true, 30), Duration::from_micros(3));
    let counters = ledger.get("scene").unwrap();
    // Revisions 1 -> 4 between two shown frames: 1 presented + 2 discarded.
    assert_eq!((counters.presented, counters.discarded), (2, 2));
    assert_eq!(counters.last_presented_us, Some(3));
    assert_eq!(counters.upload_bytes_total, 40);
    assert_eq!(counters.damage_px_total, 20);
    assert_eq!(counters.frames, 3);

    // Hidden updates are not presented; a later shown one supersedes them.
    ledger.resolve(&source(5, false, 1), Duration::from_micros(4));
    ledger.resolve(&source(7, true, 1), Duration::from_micros(5));
    let counters = ledger.get("scene").unwrap();
    assert_eq!((counters.presented, counters.discarded), (3, 4));

    // Revisions never shown before unregistering are discarded.
    ledger.resolve(&source(9, false, 0), Duration::from_micros(6));
    let gone = ledger.unregister("scene").unwrap();
    assert_eq!((gone.presented, gone.discarded), (3, 6));
    assert!(ledger.get("scene").is_none());
    assert_eq!(ledger.register("scene"), 2, "registrations are counted");
    ledger.resolve(&source(1, true, 0), Duration::from_micros(7));
    assert_eq!(ledger.get("scene").unwrap().presented, 1);
}

#[test]
fn unregistered_sources_are_ignored() {
    let mut ledger = SourceLedger::default();
    ledger.resolve(&source(1, true, 10), Duration::ZERO);
    assert!(ledger.get("scene").is_none());
    assert!(ledger.unregister("scene").is_none());
}
