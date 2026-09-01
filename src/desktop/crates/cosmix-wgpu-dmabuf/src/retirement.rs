use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
    time::Duration,
};

use bevy::render::renderer::{RenderDevice, RenderQueue};
use thiserror::Error;

/// The single bounded GPU wait used for each retirement batch.
pub const RETIREMENT_WAIT_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RetirementSequence(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RetirementBatchId(pub u64);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RetirementWaitError {
    #[error("GPU retirement wait timed out")]
    Timeout,
    #[error("GPU retirement wait failed: {0}")]
    Failed(String),
}

/// Renderer-specific seam used by the retirement worker and offline fakes.
pub trait WaitForSubmittedWork: Send + 'static {
    /// Wait once for the most recently submitted GPU work.
    fn wait_for_submitted_work(&mut self, timeout: Duration) -> Result<(), RetirementWaitError>;
}

/// Production adapter over the same Bevy/wgpu device as rendering.
///
/// Before an explicit release callback fires, all GPU work that could reference
/// that buffer has already been submitted. Ownership barriers are recorded in
/// wgpu-owned command buffers, so exact submission indices cover them through
/// the same queue authority as ordinary renderer work.
pub struct WgpuWaitForSubmittedWork {
    device: RenderDevice,
    queue: Option<RenderQueue>,
}

impl WgpuWaitForSubmittedWork {
    pub fn new(device: RenderDevice) -> Self {
        Self {
            device,
            queue: None,
        }
    }

    /// Production adapter that also flushes wgpu's deferred queue actions
    /// before taking the submitted-work snapshot.
    pub fn with_queue(device: RenderDevice, queue: RenderQueue) -> Self {
        Self {
            device,
            queue: Some(queue),
        }
    }

    /// Wait for one exact wgpu submission. Capture destinations use this for
    /// both the copy and the subsequent FOREIGN release submission.
    pub fn wait_for_submission(
        &self,
        submission: wgpu::SubmissionIndex,
        timeout: Duration,
    ) -> Result<(), RetirementWaitError> {
        poll_submitted_work(
            &self.device,
            wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(timeout),
            },
        )
    }
}

fn submitted_work_wait(timeout: Duration) -> wgpu::PollType {
    wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(timeout),
    }
}

impl WaitForSubmittedWork for WgpuWaitForSubmittedWork {
    fn wait_for_submitted_work(&mut self, timeout: Duration) -> Result<(), RetirementWaitError> {
        if let Some(queue) = &self.queue {
            queue.submit(std::iter::empty());
        }
        poll_submitted_work(&self.device, submitted_work_wait(timeout))
    }
}

fn poll_submitted_work(
    device: &RenderDevice,
    poll_type: wgpu::PollType,
) -> Result<(), RetirementWaitError> {
    let status = device.poll(poll_type).map_err(|error| match error {
        wgpu::PollError::Timeout => RetirementWaitError::Timeout,
        wgpu::PollError::WrongSubmissionIndex(requested, last_successful) => {
            RetirementWaitError::Failed(format!(
                "wgpu reported a wrong submission index ({requested}; last successful \
                 {last_successful})"
            ))
        }
    })?;
    match status {
        wgpu::PollStatus::QueueEmpty | wgpu::PollStatus::WaitSucceeded => Ok(()),
        wgpu::PollStatus::Poll => Err(RetirementWaitError::Failed(
            "blocking submitted-work wait returned poll status".into(),
        )),
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RetirementWorkerError {
    #[error(transparent)]
    Wait(#[from] RetirementWaitError),
    #[error("GPU retirement adapter panicked")]
    AdapterPanicked,
    #[error("GPU retirement batch identifier space was exhausted")]
    BatchIdExhausted,
    #[error("GPU retirement requests were not monotonic ({previous:?} then {next:?})")]
    NonMonotonicSequence {
        previous: RetirementSequence,
        next: RetirementSequence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetirementWorkerReport {
    pub batch_id: RetirementBatchId,
    pub high_water: RetirementSequence,
    pub result: Result<(), RetirementWorkerError>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RetirementRequestError {
    #[error("GPU retirement request queue is full")]
    Full,
    #[error("GPU retirement worker is unavailable")]
    Disconnected,
}

/// The sole request endpoint. It is deliberately not cloneable: dropping it
/// closes the worker's blocking receive without a wake timer or poll loop.
pub struct RetirementRequestSender {
    sender: SyncSender<RetirementSequence>,
}

impl RetirementRequestSender {
    pub fn try_send(&self, sequence: RetirementSequence) -> Result<(), RetirementRequestError> {
        self.sender.try_send(sequence).map_err(|error| match error {
            TrySendError::Full(_) => RetirementRequestError::Full,
            TrySendError::Disconnected(_) => RetirementRequestError::Disconnected,
        })
    }
}

pub struct RetirementWorker {
    thread: Option<JoinHandle<()>>,
}

impl RetirementWorker {
    pub fn join(&mut self) -> thread::Result<()> {
        match self.thread.take() {
            Some(thread) => thread.join(),
            None => Ok(()),
        }
    }

    /// Discard the join handle without waiting. The sole request sender is the
    /// worker's shutdown authority; protocol teardown must close that first.
    pub fn detach(&mut self) {
        self.thread.take();
    }
}

pub fn spawn_retirement_worker<F>(
    adapter: Box<dyn WaitForSubmittedWork>,
    request_capacity: usize,
    mut report: F,
) -> std::io::Result<(RetirementRequestSender, RetirementWorker)>
where
    F: FnMut(RetirementWorkerReport) -> bool + Send + 'static,
{
    assert!(
        request_capacity > 0,
        "retirement queue must be bounded above zero"
    );
    let (sender, receiver) = mpsc::sync_channel(request_capacity);
    let thread = thread::Builder::new()
        .name("cosmix-gpu-retirement".into())
        .spawn(move || run_retirement_worker(adapter, receiver, &mut report))?;
    Ok((
        RetirementRequestSender { sender },
        RetirementWorker {
            thread: Some(thread),
        },
    ))
}

fn run_retirement_worker<F>(
    adapter: Box<dyn WaitForSubmittedWork>,
    receiver: Receiver<RetirementSequence>,
    report: &mut F,
) where
    F: FnMut(RetirementWorkerReport) -> bool,
{
    run_retirement_worker_from(adapter, receiver, report, 1);
}

fn run_retirement_worker_from<F>(
    mut adapter: Box<dyn WaitForSubmittedWork>,
    receiver: Receiver<RetirementSequence>,
    report: &mut F,
    mut next_batch_id: u64,
) where
    F: FnMut(RetirementWorkerReport) -> bool,
{
    // A batch waits once for the wgpu submission frontier captured after all
    // requests in that batch were received. The wait may delay one concurrent
    // render submission until success, failure, or the 250 ms timeout. Failures
    // are terminal and never retried. Requests arriving after the frontier form
    // a later batch, so sustained successful batches may delay successive render
    // submissions, but neither protocol dispatch nor teardown waits on the GPU.
    while let Ok(first) = receiver.recv() {
        let mut high_water = first;
        let mut sequence_error = None;
        loop {
            match receiver.try_recv() {
                Ok(sequence) if sequence > high_water => high_water = sequence,
                Ok(sequence) => {
                    sequence_error = Some(RetirementWorkerError::NonMonotonicSequence {
                        previous: high_water,
                        next: sequence,
                    });
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        let batch_id = RetirementBatchId(next_batch_id);
        let Some(following_batch_id) = next_batch_id.checked_add(1) else {
            let _ = report(RetirementWorkerReport {
                batch_id,
                high_water,
                result: Err(RetirementWorkerError::BatchIdExhausted),
            });
            return;
        };
        next_batch_id = following_batch_id;

        let result = match sequence_error {
            Some(error) => Err(error),
            None => catch_unwind(AssertUnwindSafe(|| {
                adapter.wait_for_submitted_work(RETIREMENT_WAIT_TIMEOUT)
            }))
            .map_err(|_| RetirementWorkerError::AdapterPanicked)
            .and_then(|result| result.map_err(RetirementWorkerError::Wait)),
        };
        let failed = result.is_err();
        if !report(RetirementWorkerReport {
            batch_id,
            high_water,
            result,
        }) || failed
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use bevy::render::renderer::{RenderDevice, RenderQueue, WgpuWrapper};

    use super::*;

    struct RecordingAdapter {
        calls: Arc<AtomicUsize>,
        outcome: Result<(), RetirementWaitError>,
        entered: Option<mpsc::SyncSender<()>>,
        release: Option<Arc<Barrier>>,
    }

    impl WaitForSubmittedWork for RecordingAdapter {
        fn wait_for_submitted_work(
            &mut self,
            timeout: Duration,
        ) -> Result<(), RetirementWaitError> {
            assert_eq!(timeout, RETIREMENT_WAIT_TIMEOUT);
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = self.entered.take() {
                entered.send(()).expect("observe wait entry");
            }
            if let Some(release) = self.release.take() {
                release.wait();
            }
            self.outcome.clone()
        }
    }

    #[test]
    fn production_adapter_flushes_and_waits_on_a_noop_device_offline() {
        let (device, queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let mut adapter = WgpuWaitForSubmittedWork::with_queue(
            RenderDevice::from(device),
            RenderQueue(Arc::new(WgpuWrapper::new(queue))),
        );

        adapter
            .wait_for_submitted_work(RETIREMENT_WAIT_TIMEOUT)
            .expect("noop submitted-work wait completes");
    }

    #[test]
    fn production_adapter_waits_for_the_exact_copy_submission() {
        let (device, queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let submission = queue.submit(std::iter::empty());
        let adapter = WgpuWaitForSubmittedWork::new(RenderDevice::from(device));

        adapter
            .wait_for_submission(submission, RETIREMENT_WAIT_TIMEOUT)
            .expect("noop exact-submission wait completes");
    }

    #[test]
    fn submitted_work_wait_is_bounded_and_uses_the_latest_successful_submission() {
        match submitted_work_wait(RETIREMENT_WAIT_TIMEOUT) {
            wgpu::PollType::Wait {
                submission_index,
                timeout,
            } => {
                assert!(submission_index.is_none());
                assert_eq!(timeout, Some(RETIREMENT_WAIT_TIMEOUT));
            }
            wgpu::PollType::Poll => panic!("retirement uses one bounded blocking wait"),
        }
    }

    #[test]
    fn requests_arriving_during_a_wait_form_the_next_batch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
        let release = Arc::new(Barrier::new(2));
        let (report_sender, report_receiver) = mpsc::channel();
        let (requests, mut worker) = spawn_retirement_worker(
            Box::new(RecordingAdapter {
                calls: Arc::clone(&calls),
                outcome: Ok(()),
                entered: Some(entered_sender),
                release: Some(Arc::clone(&release)),
            }),
            4,
            move |event| report_sender.send(event).is_ok(),
        )
        .expect("spawn worker");

        requests
            .try_send(RetirementSequence(1))
            .expect("first request");
        entered_receiver.recv().expect("first wait entered");
        requests
            .try_send(RetirementSequence(2))
            .expect("request during wait");
        release.wait();

        let first = report_receiver.recv().expect("first report");
        let second = report_receiver.recv().expect("second report");
        assert_eq!(first.high_water, RetirementSequence(1));
        assert_eq!(second.high_water, RetirementSequence(2));
        assert_eq!(first.batch_id, RetirementBatchId(1));
        assert_eq!(second.batch_id, RetirementBatchId(2));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(requests);
        worker.join().expect("worker exits after sender drop");
    }

    #[test]
    fn requests_already_queued_share_one_wait_and_report() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::sync_channel(2);
        sender.send(RetirementSequence(1)).expect("first request");
        sender.send(RetirementSequence(2)).expect("second request");
        drop(sender);
        let mut reports = Vec::new();

        run_retirement_worker(
            Box::new(RecordingAdapter {
                calls: Arc::clone(&calls),
                outcome: Ok(()),
                entered: None,
                release: None,
            }),
            receiver,
            &mut |report| {
                reports.push(report);
                true
            },
        );

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].high_water, RetirementSequence(2));
    }

    #[test]
    fn a_wait_failure_is_reported_once_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (report_sender, report_receiver) = mpsc::channel();
        let (requests, mut worker) = spawn_retirement_worker(
            Box::new(RecordingAdapter {
                calls: Arc::clone(&calls),
                outcome: Err(RetirementWaitError::Timeout),
                entered: None,
                release: None,
            }),
            4,
            move |event| report_sender.send(event).is_ok(),
        )
        .expect("spawn worker");

        requests.try_send(RetirementSequence(1)).expect("request");
        let report = report_receiver.recv().expect("failure report");
        assert_eq!(
            report.result,
            Err(RetirementWorkerError::Wait(RetirementWaitError::Timeout))
        );
        worker.join().expect("faulted worker exits");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            requests.try_send(RetirementSequence(2)),
            Err(RetirementRequestError::Disconnected)
        );
    }

    #[test]
    fn an_adapter_panic_becomes_a_terminal_worker_report() {
        struct Panics;
        impl WaitForSubmittedWork for Panics {
            fn wait_for_submitted_work(
                &mut self,
                _timeout: Duration,
            ) -> Result<(), RetirementWaitError> {
                panic!("simulated device loss")
            }
        }

        let (report_sender, report_receiver) = mpsc::channel();
        let (requests, mut worker) = spawn_retirement_worker(Box::new(Panics), 1, move |event| {
            report_sender.send(event).is_ok()
        })
        .expect("spawn worker");
        requests.try_send(RetirementSequence(1)).expect("request");

        assert_eq!(
            report_receiver.recv().expect("panic report").result,
            Err(RetirementWorkerError::AdapterPanicked)
        );
        worker.join().expect("panic was contained by worker");
    }

    #[test]
    fn request_queue_is_bounded() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let requests = RetirementRequestSender { sender };
        requests
            .try_send(RetirementSequence(1))
            .expect("first request fills queue");
        assert_eq!(
            requests.try_send(RetirementSequence(2)),
            Err(RetirementRequestError::Full)
        );
    }

    #[test]
    fn non_monotonic_requests_fault_without_waiting_for_submitted_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::sync_channel(2);
        sender.send(RetirementSequence(2)).expect("first request");
        sender
            .send(RetirementSequence(1))
            .expect("regressed request");
        drop(sender);
        let mut reports = Vec::new();

        run_retirement_worker(
            Box::new(RecordingAdapter {
                calls: Arc::clone(&calls),
                outcome: Ok(()),
                entered: None,
                release: None,
            }),
            receiver,
            &mut |report| {
                reports.push(report);
                true
            },
        );

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(reports.len(), 1);
        assert_eq!(
            reports[0].result,
            Err(RetirementWorkerError::NonMonotonicSequence {
                previous: RetirementSequence(2),
                next: RetirementSequence(1),
            })
        );
    }

    #[test]
    fn exhausted_batch_id_faults_without_waiting_for_submitted_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.send(RetirementSequence(1)).expect("request");
        drop(sender);
        let mut reports = Vec::new();

        run_retirement_worker_from(
            Box::new(RecordingAdapter {
                calls: Arc::clone(&calls),
                outcome: Ok(()),
                entered: None,
                release: None,
            }),
            receiver,
            &mut |report| {
                reports.push(report);
                true
            },
            u64::MAX,
        );

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].batch_id, RetirementBatchId(u64::MAX));
        assert_eq!(reports[0].high_water, RetirementSequence(1));
        assert_eq!(
            reports[0].result,
            Err(RetirementWorkerError::BatchIdExhausted)
        );
    }

    #[test]
    fn detaching_never_waits_for_an_inflight_gpu_wait() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_sender, entered_receiver) = mpsc::sync_channel(1);
        let release = Arc::new(Barrier::new(2));
        let (requests, mut worker) = spawn_retirement_worker(
            Box::new(RecordingAdapter {
                calls,
                outcome: Ok(()),
                entered: Some(entered_sender),
                release: Some(Arc::clone(&release)),
            }),
            1,
            |_| true,
        )
        .expect("spawn worker");
        requests.try_send(RetirementSequence(1)).expect("request");
        entered_receiver.recv().expect("wait entered");

        drop(requests);
        worker.detach();
        release.wait();
    }
}
