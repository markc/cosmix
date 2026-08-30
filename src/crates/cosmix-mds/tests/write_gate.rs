//! Fair in-process writer-queue coverage.
//!
//! Each writer owns a different set connection, so the per-set
//! mutexes do not serialize them. Every `BEGIN IMMEDIATE` still needs
//! the one ATTACH'd `blobs.sqlite` write lock. The shared store's fair
//! gate must give every writer a bounded turn instead of leaving
//! scheduling to SQLite's non-FIFO busy-handler retry loop.

use cosmix_mds::{Error, Mds, SetId, SqliteCasMds};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const N_WRITERS: usize = 8;
const ROUNDS: usize = 6;
const HOLD_MS: u64 = 10;
const FAIR_WAIT_BOUND: Duration = Duration::from_secs(1);

#[test]
fn every_writer_gets_a_bounded_turn_at_the_fair_gate() {
    let root = TempDir::new().expect("tempdir");
    let mds = Arc::new(SqliteCasMds::open(root.path()).expect("open mds"));
    let sets: Vec<SetId> = (0..N_WRITERS)
        .map(|_| {
            let set = SetId(uuid::Uuid::now_v7());
            mds.create_set(&set).expect("create set");
            set
        })
        .collect();
    let barrier = Arc::new(Barrier::new(N_WRITERS));

    let handles: Vec<_> = sets
        .into_iter()
        .map(|set| {
            let mds = Arc::clone(&mds);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut max_wait = Duration::ZERO;
                for _ in 0..ROUNDS {
                    barrier.wait();
                    let started = Instant::now();
                    mds.with_set_tx(&set, |handle| {
                        handle
                            .tx()
                            .execute(
                                "UPDATE set_state SET set_change_seq = set_change_seq + 1 \
                                 WHERE set_id = ?1",
                                [set.0.to_string()],
                            )
                            .map_err(|e| Error::Other(format!("bump set_change_seq: {e}")))?;
                        // Make one turn's service time visible and
                        // stable enough to bound the whole FIFO queue.
                        thread::sleep(Duration::from_millis(HOLD_MS));
                        Ok(())
                    })
                    .expect("fair-gated write");
                    let wait = started
                        .elapsed()
                        .saturating_sub(Duration::from_millis(HOLD_MS));
                    max_wait = max_wait.max(wait);
                }
                max_wait
            })
        })
        .collect();

    let max_waits: Vec<Duration> = handles
        .into_iter()
        .map(|handle| handle.join().expect("writer join"))
        .collect();
    let ideal_queue = Duration::from_millis(N_WRITERS as u64 * HOLD_MS);

    for (writer, max_wait) in max_waits.into_iter().enumerate() {
        assert!(
            max_wait < FAIR_WAIT_BOUND,
            "writer {writer} waited {max_wait:?}; fair queue ideal is about {ideal_queue:?} \
             and the generous bound is {FAIR_WAIT_BOUND:?}"
        );
    }
}
