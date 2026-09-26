//! Worker spawning for dopus: thread-per-job replacements for filemgr's
//! detached `IoTaskPool` tasks (browser.rs:1385-1396, 1410-1421, 1654-1671,
//! 1818-1828). Same shape — one detached job per listing/count/operation —
//! with plain `std::thread::spawn` and one shared `mpsc::Sender` per reply
//! type merged into a single `CoreEvent` channel. No relay thread: jobs send
//! directly on the app-visible channel, so there is exactly one hop between
//! worker and view and nothing to re-plumb when a futures channel arrives.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;

use crate::events::CoreEvent;
use crate::model::{count_directory_entries, read_directory, CountJob, PaneId};
use crate::ops::FileOperation;

#[derive(Clone)]
pub(crate) struct WorkerHandle {
    tx: mpsc::Sender<CoreEvent>,
    /// Per-pane listing generations, mirrored from the core on every
    /// `start_listing` so count jobs can re-check liveness without touching
    /// core state (browser.rs:1367).
    pub(crate) generations: [Arc<AtomicU64>; 2],
    /// Count jobs currently executing. Shared with the workers because stale
    /// jobs retire without ever sending a reply, so only the worker itself
    /// can release its slot (browser.rs:338, 1669).
    pub(crate) count_in_flight: Arc<AtomicUsize>,
}

impl WorkerHandle {
    pub(crate) fn new(tx: mpsc::Sender<CoreEvent>) -> Self {
        Self {
            tx,
            generations: [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))],
            count_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn store_generation(&self, pane: PaneId, generation: u64) {
        self.generations[pane.index()].store(generation, Ordering::Release);
    }

    /// One root or child listing per thread (browser.rs:1385-1396, 1410-1421).
    pub(crate) fn spawn_listing(
        &self,
        pane: PaneId,
        generation: u64,
        path: PathBuf,
        root: bool,
        show_hidden: bool,
    ) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = read_directory(&path, show_hidden);
            let _ = tx.send(CoreEvent::ListingArrived {
                pane,
                generation,
                path,
                root,
                result,
            });
        });
    }

    /// One directory count per thread. The generation is re-checked before
    /// the walk, per entry through the `cancelled()` closure, and before the
    /// send — verbatim from browser.rs:1654-1671.
    pub(crate) fn spawn_count(&self, job: CountJob) {
        let live_generation = Arc::clone(&self.generations[job.pane.index()]);
        let tx = self.tx.clone();
        let in_flight = Arc::clone(&self.count_in_flight);
        in_flight.fetch_add(1, Ordering::AcqRel);
        thread::spawn(move || {
            let CountJob {
                pane,
                generation,
                entry_path,
                show_hidden,
            } = job;
            if live_generation.load(Ordering::Acquire) == generation {
                let count = count_directory_entries(&entry_path, show_hidden, || {
                    live_generation.load(Ordering::Acquire) != generation
                });
                if live_generation.load(Ordering::Acquire) == generation {
                    let _ = tx.send(CoreEvent::CountArrived {
                        pane,
                        generation,
                        path: entry_path,
                        count,
                    });
                }
            }
            in_flight.fetch_sub(1, Ordering::AcqRel);
        });
    }

    /// One file operation per thread (browser.rs:1818-1828).
    pub(crate) fn spawn_operation(&self, operation: FileOperation, source_pane: PaneId) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let kind = operation.kind;
            let result = operation.execute();
            let _ = tx.send(CoreEvent::OperationArrived {
                kind,
                source_pane,
                result,
            });
        });
    }
}
