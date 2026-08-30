//! Offline KMS render-worker control and generation-aware source registration.
//!
//! Commands and events use unbounded `std::sync::mpsc` channels: every
//! generation transition is delivered in order or the channel disconnects.
//! Platform work runs only on the named worker and never on `cosmix-wayland`.

use std::{
    collections::BTreeMap,
    fmt,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use bevy::{camera::ManualTextureViewHandle, render::texture::ManualTextureView};

use super::{
    kms::{KmsRenderCommand, KmsRenderOperation, KmsRenderReply, OutputKey, SelectedOutput},
    render::{
        AcquiredOutputFrame, PresentOutcome, fallible_present_output_frame, present_output_frame,
    },
};

#[cfg(test)]
use super::render::PresentDeadline;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsRenderPlatformFailure {
    pub(crate) code: &'static str,
    pub(crate) detail: String,
    disposition: KmsRenderPlatformFailureDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KmsRenderPlatformFailureDisposition {
    Recoverable,
    Terminal,
}

impl KmsRenderPlatformFailure {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            disposition: KmsRenderPlatformFailureDisposition::Recoverable,
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn terminal(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            disposition: KmsRenderPlatformFailureDisposition::Terminal,
        }
    }

    fn is_terminal(&self) -> bool {
        self.disposition == KmsRenderPlatformFailureDisposition::Terminal
    }

    /// A non-blocking atomic commit can lose DRM authority between its final
    /// cancellation sample and the ioctl outcome. Presentation transports
    /// these three revocation errnos to the coordinator without killing the
    /// worker so an external pause can attribute them within its bounded
    /// arbitration window. Every other terminal failure remains worker-fatal.
    pub(crate) fn atomic_commit_authority_errno(&self) -> Option<i32> {
        if !self.is_terminal()
            || !matches!(
                self.code,
                "kms-live-atomic-commit-hard-rejection"
                    | "kms-live-atomic-first-nonblocking-modeset-refused"
            )
        {
            return None;
        }
        let (_, errno_and_detail) = self.detail.split_once(" failed with errno ")?;
        let (errno, _) = errno_and_detail.split_once(':')?;
        let errno = errno.parse::<i32>().ok()?;
        matches!(errno, libc::EACCES | libc::EPERM | libc::ENODEV).then_some(errno)
    }
}

#[cfg(test)]
impl From<&str> for KmsRenderPlatformFailure {
    fn from(detail: &str) -> Self {
        Self::new("kms-render-test-failure", detail)
    }
}

pub(crate) struct RenderSource<P> {
    pub(crate) placeholder: P,
    pub(crate) acquire: Box<
        dyn FnMut() -> Result<AcquiredOutputFrame, KmsRenderPlatformFailure>
            + Send
            + Sync
            + 'static,
    >,
}

pub(crate) trait PlaceholderExtent {
    fn extent(&self) -> (u32, u32);
}

impl PlaceholderExtent for ManualTextureView {
    fn extent(&self) -> (u32, u32) {
        (self.size.x, self.size.y)
    }
}

/// Recoverable platform failures are failure-atomic: before returning `Err`, an
/// implementor must release resources acquired by that call and leave no partial
/// transition requiring a later compensating call from the worker. A terminal
/// failure may report that cleanup could not be proved; the worker then stops.
pub(crate) trait KmsRenderPlatform: Send + 'static {
    type Placeholder: PlaceholderExtent + Send + 'static;

    /// Flush and retire GPU work after render-world quiescence and before any
    /// platform operation can destroy the corresponding surface.
    fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure>;
    fn resume(&mut self, generation: u64) -> Result<(), KmsRenderPlatformFailure>;
    fn add_output(
        &mut self,
        output: &SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure>;
    fn change_output(
        &mut self,
        output: &SelectedOutput,
    ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure>;
    fn remove_output(&mut self, key: &OutputKey) -> Result<(), KmsRenderPlatformFailure>;

    /// Release platform resources only after the render world has proved that
    /// every callback, presenter and cloned texture view has gone away.
    fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
        Ok(())
    }
}

pub(crate) enum KmsRenderWorkerEvent<P> {
    CommandAccepted(KmsRenderCommand),
    SourceReady {
        generation: u64,
        output: SelectedOutput,
        source: RenderSource<P>,
    },
    Reply(KmsRenderReply),
    WorkerFailed(KmsRenderWorkerFailure),
    WorkerStopped(KmsRenderWorkerExit),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsRenderWorkerFailure {
    pub(crate) operation: KmsRenderOperation,
    pub(crate) generation: u64,
    pub(crate) key: Option<OutputKey>,
    pub(crate) failure: KmsRenderPlatformFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderWorkerExit {
    Cancelled,
    Panicked(KmsRenderWorkerFailure),
    CommandChannelDisconnected,
    ReplyChannelDisconnected {
        operation: KmsRenderOperation,
        generation: u64,
        key: Option<OutputKey>,
    },
    PlatformFailed(KmsRenderWorkerFailure),
    RenderPathDisconnected(KmsRenderWorkerFailure),
    RegistrarChannelDisconnected {
        operation: KmsRenderOperation,
        generation: u64,
        key: Option<OutputKey>,
    },
    UnexpectedRegistrarRelease {
        expected: KmsRenderRelease,
        actual: KmsRenderRelease,
    },
    RenderWorldHandoffAborted {
        operation: KmsRenderOperation,
        generation: u64,
        key: Option<OutputKey>,
    },
    RegistrationChannelDisconnected {
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
    },
    TeardownFailed {
        prior: Box<KmsRenderWorkerExit>,
        failure: KmsRenderPlatformFailure,
    },
    RenderWorldDropUnproven {
        prior: Box<KmsRenderWorkerExit>,
    },
    UnexpectedRegistrationDisposition {
        operation: KmsRenderOperation,
        expected_generation: u64,
        expected_key: OutputKey,
        actual: KmsRenderRegistration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsRenderRelease {
    pub(crate) operation: KmsRenderOperation,
    pub(crate) generation: u64,
    pub(crate) key: Option<OutputKey>,
    pub(crate) outcome: KmsRenderReleaseOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderReleaseOutcome {
    Granted,
    Aborted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsRenderQuiescence {
    pub(crate) operation: KmsRenderOperation,
    pub(crate) generation: u64,
    pub(crate) key: Option<OutputKey>,
    pub(crate) outcome: KmsRenderQuiescenceOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderQuiescenceOutcome {
    Quiesced,
    Aborted,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderLifecycleState {
    Active = 0,
    Quiescing = 1,
    Suspended = 2,
    Resuming = 3,
    Terminating = 4,
    Terminated = 5,
}

/// Fast render-path gate with states reserved for Rung F. D-2b's pause and
/// authorised-target hotplug policy chooses `Terminating`. Terminal states are
/// one-way: no delayed transition completion may make callbacks live again.
pub(crate) struct KmsRenderLifecycle(AtomicU8);

impl KmsRenderLifecycle {
    pub(crate) fn new() -> Self {
        Self(AtomicU8::new(KmsRenderLifecycleState::Active as u8))
    }

    pub(crate) fn state(&self) -> KmsRenderLifecycleState {
        match self.0.load(Ordering::Acquire) {
            0 => KmsRenderLifecycleState::Active,
            1 => KmsRenderLifecycleState::Quiescing,
            2 => KmsRenderLifecycleState::Suspended,
            3 => KmsRenderLifecycleState::Resuming,
            4 => KmsRenderLifecycleState::Terminating,
            _ => KmsRenderLifecycleState::Terminated,
        }
    }

    fn begin_quiescing(&self) {
        self.transition_nonterminal(KmsRenderLifecycleState::Quiescing);
    }

    fn suspended(&self) {
        self.transition_nonterminal(KmsRenderLifecycleState::Suspended);
    }

    fn begin_resuming(&self) {
        self.transition_nonterminal(KmsRenderLifecycleState::Resuming);
    }

    fn active(&self) {
        self.transition_nonterminal(KmsRenderLifecycleState::Active);
    }

    #[cfg(test)]
    pub(super) fn attempt_active_for_test(&self) {
        self.active();
    }

    fn begin_termination(&self) {
        self.0
            .fetch_max(KmsRenderLifecycleState::Terminating as u8, Ordering::AcqRel);
    }

    fn terminated(&self) {
        self.0
            .store(KmsRenderLifecycleState::Terminated as u8, Ordering::Release);
    }

    fn transition_nonterminal(&self, next: KmsRenderLifecycleState) {
        debug_assert!((next as u8) < KmsRenderLifecycleState::Terminating as u8);
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            if current >= KmsRenderLifecycleState::Terminating as u8 {
                return;
            }
            match self.0.compare_exchange_weak(
                current,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

/// Global, terminal-only proof. Per-transition detachment uses
/// [`KmsRenderQuiescence`] and deliberately cannot manufacture this token.
#[derive(Debug)]
pub(crate) struct RenderWorldDropped(());

#[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
pub(crate) struct RenderWorldDropAcknowledger(Option<Sender<RenderWorldDropped>>);

impl RenderWorldDropAcknowledger {
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    pub(crate) fn acknowledge(mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(RenderWorldDropped(()));
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderRegistrationDisposition {
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsRenderRegistration {
    pub(crate) generation: u64,
    pub(crate) key: OutputKey,
    pub(crate) disposition: KmsRenderRegistrationDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderSendError {
    WorkerStopped,
    CommandChannelDisconnected,
}

impl fmt::Display for KmsRenderSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerStopped => formatter.write_str("KMS render worker has stopped"),
            Self::CommandChannelDisconnected => {
                formatter.write_str("KMS render command channel disconnected")
            }
        }
    }
}

impl std::error::Error for KmsRenderSendError {}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderJoinOutcome {
    Exited(KmsRenderWorkerExit),
    Panicked,
    /// The diagnostic deadline elapsed. Offline workers are joined before
    /// return; guarded live workers detach while retaining their thread-owned
    /// platform and lease until they can exit safely.
    TimedOut,
}

enum WorkerInbound<T> {
    Value(T),
    Cancel,
}

pub(crate) struct KmsRenderInputSender<T>(Sender<WorkerInbound<T>>);

impl<T> Clone for KmsRenderInputSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> KmsRenderInputSender<T> {
    pub(crate) fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        match self.0.send(WorkerInbound::Value(value)) {
            Ok(()) => Ok(()),
            Err(mpsc::SendError(WorkerInbound::Value(value))) => Err(mpsc::SendError(value)),
            Err(mpsc::SendError(WorkerInbound::Cancel)) => {
                unreachable!("payload send cannot return a cancellation envelope")
            }
        }
    }

    fn cancel(&self) {
        let _ = self.0.send(WorkerInbound::Cancel);
    }

    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, KmsRenderInputReceiver<T>) {
        let (sender, receiver) = mpsc::channel();
        (Self(sender), KmsRenderInputReceiver(receiver))
    }
}

#[cfg(test)]
pub(crate) struct KmsRenderInputReceiver<T>(Receiver<WorkerInbound<T>>);

#[cfg(test)]
impl<T> KmsRenderInputReceiver<T> {
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, &'static str> {
        match self.0.recv_timeout(timeout) {
            Ok(WorkerInbound::Value(value)) => Ok(value),
            Ok(WorkerInbound::Cancel) => Err("unexpected cancellation"),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("timed out"),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("disconnected"),
        }
    }
}

pub(crate) struct KmsRenderWorker<P> {
    commands: Option<KmsRenderInputSender<KmsRenderCommand>>,
    releases: Option<KmsRenderInputSender<KmsRenderRelease>>,
    registrations: Option<KmsRenderInputSender<KmsRenderRegistration>>,
    quiescences: Option<KmsRenderInputSender<KmsRenderQuiescence>>,
    completion: Receiver<KmsRenderWorkerExit>,
    thread: Option<JoinHandle<KmsRenderWorkerExit>>,
    cancelled: Arc<AtomicBool>,
    lifecycle: Arc<KmsRenderLifecycle>,
    ingress: Arc<Mutex<()>>,
    terminal_failure: Arc<Mutex<Option<KmsRenderWorkerFailure>>>,
    detach_on_timeout: bool,
    placeholder: PhantomData<P>,
}

#[derive(Clone)]
pub(crate) struct KmsRenderWorkerStop {
    commands: KmsRenderInputSender<KmsRenderCommand>,
    releases: KmsRenderInputSender<KmsRenderRelease>,
    registrations: KmsRenderInputSender<KmsRenderRegistration>,
    quiescences: KmsRenderInputSender<KmsRenderQuiescence>,
    cancelled: Arc<AtomicBool>,
    lifecycle: Arc<KmsRenderLifecycle>,
    ingress: Arc<Mutex<()>>,
    terminal_failure: Arc<Mutex<Option<KmsRenderWorkerFailure>>>,
}

impl KmsRenderWorkerStop {
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    pub(crate) fn begin_shutdown(&self) {
        let _ingress = self
            .ingress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.cancelled.store(true, Ordering::Release);
        self.lifecycle.begin_termination();
    }

    pub(crate) fn begin_render_path_failure(&self, failure: KmsRenderWorkerFailure) {
        let _ingress = self
            .ingress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut terminal_failure = self
            .terminal_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if terminal_failure.is_none() {
            *terminal_failure = Some(failure);
        }
        self.cancelled.store(true, Ordering::Release);
        self.lifecycle.begin_termination();
    }

    pub(crate) fn wake(&self) {
        self.commands.cancel();
        self.releases.cancel();
        self.registrations.cancel();
        self.quiescences.cancel();
    }

    pub(super) fn render_lifecycle(&self) -> Arc<KmsRenderLifecycle> {
        Arc::clone(&self.lifecycle)
    }
}

impl<P> KmsRenderWorker<P>
where
    P: PlaceholderExtent + Send + 'static,
{
    pub(crate) fn spawn<T>(
        platform: T,
        event_sender: Sender<KmsRenderWorkerEvent<P>>,
    ) -> Result<Self, std::io::Error>
    where
        T: KmsRenderPlatform<Placeholder = P>,
    {
        Self::spawn_inner(platform, event_sender, None)
    }

    /// Live platforms use this constructor. The worker retains `platform` on
    /// every normal, panic and disconnected exit until `acknowledge` is called
    /// after the render sub-app has actually been dropped.
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    pub(crate) fn spawn_guarded<T>(
        platform: T,
        event_sender: Sender<KmsRenderWorkerEvent<P>>,
    ) -> Result<(Self, RenderWorldDropAcknowledger), std::io::Error>
    where
        T: KmsRenderPlatform<Placeholder = P>,
    {
        let (sender, receiver) = mpsc::channel();
        let worker = Self::spawn_inner(platform, event_sender, Some(receiver))?;
        Ok((worker, RenderWorldDropAcknowledger(Some(sender))))
    }

    fn spawn_inner<T>(
        platform: T,
        event_sender: Sender<KmsRenderWorkerEvent<P>>,
        render_world_dropped: Option<Receiver<RenderWorldDropped>>,
    ) -> Result<Self, std::io::Error>
    where
        T: KmsRenderPlatform<Placeholder = P>,
    {
        let detach_on_timeout = render_world_dropped.is_some();
        let (command_sender, commands) = mpsc::channel();
        let command_sender = KmsRenderInputSender(command_sender);
        let (release_sender, releases) = mpsc::channel();
        let release_sender = KmsRenderInputSender(release_sender);
        let (registration_sender, registrations) = mpsc::channel();
        let registration_sender = KmsRenderInputSender(registration_sender);
        let (quiescence_sender, quiescences) = mpsc::channel();
        let quiescence_sender = KmsRenderInputSender(quiescence_sender);
        let worker_events = event_sender.clone();
        let (completion_sender, completion) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let lifecycle = Arc::new(KmsRenderLifecycle::new());
        let worker_lifecycle = Arc::clone(&lifecycle);
        let ingress = Arc::new(Mutex::new(()));
        let worker_ingress = Arc::clone(&ingress);
        let terminal_failure = Arc::new(Mutex::new(None));
        let worker_terminal_failure = Arc::clone(&terminal_failure);
        let thread = thread::Builder::new()
            .name("cosmix-kms-render".into())
            .spawn(move || {
                let exit = run_worker(
                    platform,
                    KmsRenderWorkerInputs {
                        commands,
                        releases,
                        registrations,
                        quiescences,
                        events: worker_events,
                    },
                    KmsRenderWorkerControl {
                        cancelled: worker_cancelled,
                        ingress: worker_ingress,
                        terminal_failure: worker_terminal_failure,
                        lifecycle: worker_lifecycle,
                        render_world_dropped,
                    },
                );
                let _ = completion_sender.send(exit.clone());
                exit
            })?;
        Ok(Self {
            commands: Some(command_sender),
            releases: Some(release_sender),
            registrations: Some(registration_sender),
            quiescences: Some(quiescence_sender),
            completion,
            thread: Some(thread),
            cancelled,
            lifecycle,
            ingress,
            terminal_failure,
            detach_on_timeout,
            placeholder: PhantomData,
        })
    }

    pub(crate) fn send(&self, command: KmsRenderCommand) -> Result<(), KmsRenderSendError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(KmsRenderSendError::WorkerStopped);
        }
        let _ingress = self
            .ingress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.cancelled.load(Ordering::Acquire) {
            return Err(KmsRenderSendError::WorkerStopped);
        }
        self.commands
            .as_ref()
            .ok_or(KmsRenderSendError::CommandChannelDisconnected)?
            .send(command)
            .map_err(|_| KmsRenderSendError::CommandChannelDisconnected)
    }

    pub(crate) fn release_sender(&self) -> KmsRenderInputSender<KmsRenderRelease> {
        self.releases
            .as_ref()
            .expect("release sender exists while the worker is running")
            .clone()
    }

    pub(crate) fn registration_sender(&self) -> KmsRenderInputSender<KmsRenderRegistration> {
        self.registrations
            .as_ref()
            .expect("registration sender exists while the worker is running")
            .clone()
    }

    pub(crate) fn quiescence_sender(&self) -> KmsRenderInputSender<KmsRenderQuiescence> {
        self.quiescences
            .as_ref()
            .expect("quiescence sender exists while the worker is running")
            .clone()
    }

    pub(crate) fn stop_handle(&self) -> KmsRenderWorkerStop {
        KmsRenderWorkerStop {
            commands: self
                .commands
                .as_ref()
                .expect("command sender exists while the worker is running")
                .clone(),
            releases: self.release_sender(),
            registrations: self.registration_sender(),
            quiescences: self.quiescence_sender(),
            cancelled: Arc::clone(&self.cancelled),
            lifecycle: Arc::clone(&self.lifecycle),
            ingress: Arc::clone(&self.ingress),
            terminal_failure: Arc::clone(&self.terminal_failure),
        }
    }

    pub(crate) fn finish(mut self, timeout: Duration) -> KmsRenderJoinOutcome {
        {
            let _ingress = self
                .ingress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.cancelled.store(true, Ordering::Release);
            self.lifecycle.begin_termination();
        }
        if let Some(commands) = &self.commands {
            commands.cancel();
        }
        if let Some(releases) = &self.releases {
            releases.cancel();
        }
        if let Some(registrations) = &self.registrations {
            registrations.cancel();
        }
        if let Some(quiescences) = &self.quiescences {
            quiescences.cancel();
        }
        self.commands.take();
        self.releases.take();
        self.registrations.take();
        self.quiescences.take();
        let Some(thread) = self.thread.take() else {
            return KmsRenderJoinOutcome::Panicked;
        };
        match self.completion.recv_timeout(timeout) {
            Ok(exit) => match thread.join() {
                Ok(joined) if joined == exit => KmsRenderJoinOutcome::Exited(exit),
                Ok(joined) => KmsRenderJoinOutcome::Exited(joined),
                Err(_) => KmsRenderJoinOutcome::Panicked,
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
                Ok(exit) => KmsRenderJoinOutcome::Exited(exit),
                Err(_) => KmsRenderJoinOutcome::Panicked,
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if self.detach_on_timeout {
                    // A guarded worker owns the live lease and platform in its
                    // thread. Detaching preserves that ownership fail-closed
                    // while allowing protocol and session teardown to proceed;
                    // the thread destroys the platform only after its blocked
                    // driver call returns and RenderWorldDropped was received.
                    drop(thread);
                    KmsRenderJoinOutcome::TimedOut
                } else {
                    // Offline workers retain D-2a's stronger no-detach rule.
                    match thread.join() {
                        Ok(_) => KmsRenderJoinOutcome::TimedOut,
                        Err(_) => KmsRenderJoinOutcome::Panicked,
                    }
                }
            }
        }
    }
}

struct KmsRenderWorkerInputs<P> {
    commands: Receiver<WorkerInbound<KmsRenderCommand>>,
    releases: Receiver<WorkerInbound<KmsRenderRelease>>,
    registrations: Receiver<WorkerInbound<KmsRenderRegistration>>,
    quiescences: Receiver<WorkerInbound<KmsRenderQuiescence>>,
    events: Sender<KmsRenderWorkerEvent<P>>,
}

struct KmsRenderWorkerControl {
    cancelled: Arc<AtomicBool>,
    ingress: Arc<Mutex<()>>,
    terminal_failure: Arc<Mutex<Option<KmsRenderWorkerFailure>>>,
    lifecycle: Arc<KmsRenderLifecycle>,
    render_world_dropped: Option<Receiver<RenderWorldDropped>>,
}

fn run_worker<T>(
    mut platform: T,
    inputs: KmsRenderWorkerInputs<T::Placeholder>,
    control: KmsRenderWorkerControl,
) -> KmsRenderWorkerExit
where
    T: KmsRenderPlatform,
{
    let current = Mutex::new(None);
    let mut exit = catch_unwind(AssertUnwindSafe(|| {
        run_worker_loop(&mut platform, &inputs, &control, &current)
    }))
    .unwrap_or_else(|_| KmsRenderWorkerExit::Panicked(panicked_worker_failure(&current)));
    finalize_worker_exit(
        &inputs.events,
        &inputs.commands,
        &control.cancelled,
        &control.ingress,
        &control.terminal_failure,
        &exit,
    );
    if let Some(render_world_dropped) = &control.render_world_dropped {
        // This wait is deliberately on the render worker. The Smithay protocol
        // frontend owns another thread and remains dispatchable while Bevy or
        // the driver is blocked during teardown.
        if render_world_dropped.recv().is_err() {
            // Channel closure is not proof that Bevy dropped its world. Leak
            // the platform fail-closed instead of destroying DRM resources
            // underneath an unproven callback or cloned texture view.
            tracing::error!(
                "render-world teardown acknowledgement was lost; leaking live KMS platform"
            );
            std::mem::forget(platform);
            exit = KmsRenderWorkerExit::RenderWorldDropUnproven {
                prior: Box::new(exit),
            };
            control.lifecycle.terminated();
            return exit;
        }
        if let Err(failure) = platform.teardown() {
            tracing::error!(code = failure.code, detail = %failure.detail, "live KMS teardown failed");
            exit = KmsRenderWorkerExit::TeardownFailed {
                prior: Box::new(exit),
                failure,
            };
        }
    }
    control.lifecycle.terminated();
    exit
}

fn run_worker_loop<T>(
    platform: &mut T,
    inputs: &KmsRenderWorkerInputs<T::Placeholder>,
    control: &KmsRenderWorkerControl,
    current: &Mutex<Option<KmsRenderWorkerContext>>,
) -> KmsRenderWorkerExit
where
    T: KmsRenderPlatform,
{
    let KmsRenderWorkerInputs {
        commands,
        releases,
        registrations,
        quiescences,
        events,
    } = inputs;
    let KmsRenderWorkerControl {
        cancelled,
        terminal_failure,
        ..
    } = control;
    loop {
        let command = match commands.recv() {
            Ok(WorkerInbound::Value(command)) => command,
            Ok(WorkerInbound::Cancel) => return cancellation_exit(terminal_failure),
            Err(_) => return KmsRenderWorkerExit::CommandChannelDisconnected,
        };
        // A dequeue concurrent with finish's Release store can still win this Acquire load and
        // execute because it was accepted before shutdown began. Once cancellation is observable,
        // no dequeued command executes; the in-band Cancel wakes and terminates the next wait.
        if cancelled.load(Ordering::Acquire) {
            let exit = cancellation_exit(terminal_failure);
            publish_stopped_command_failure(events, &command, &exit);
            return exit;
        }
        let (operation, generation, key) = command_identity(&command);
        set_worker_context(current, operation, generation, key.clone());
        if events
            .send(KmsRenderWorkerEvent::CommandAccepted(command.clone()))
            .is_err()
        {
            return KmsRenderWorkerExit::ReplyChannelDisconnected {
                operation,
                generation,
                key,
            };
        }
        if matches!(
            operation,
            KmsRenderOperation::Suspend
                | KmsRenderOperation::ChangeOutput
                | KmsRenderOperation::RemoveOutput
        ) {
            control.lifecycle.begin_quiescing();
            let expected = KmsRenderRelease {
                operation,
                generation,
                key: key.clone(),
                outcome: KmsRenderReleaseOutcome::Granted,
            };
            let actual = match wait_for_handshake(releases) {
                HandshakeWait::Value(actual) => {
                    if cancelled.load(Ordering::Acquire) {
                        return cancellation_exit(terminal_failure);
                    }
                    actual
                }
                HandshakeWait::Cancelled => return cancellation_exit(terminal_failure),
                HandshakeWait::Disconnected => {
                    return KmsRenderWorkerExit::RegistrarChannelDisconnected {
                        operation,
                        generation,
                        key,
                    };
                }
            };
            if release_identity(&actual) != release_identity(&expected) {
                return KmsRenderWorkerExit::UnexpectedRegistrarRelease { expected, actual };
            }
            if actual.outcome == KmsRenderReleaseOutcome::Aborted {
                return KmsRenderWorkerExit::RenderWorldHandoffAborted {
                    operation,
                    generation,
                    key,
                };
            }
            let expected = KmsRenderQuiescence {
                operation,
                generation,
                key: key.clone(),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            };
            let actual = match wait_for_handshake(quiescences) {
                HandshakeWait::Value(actual) => actual,
                HandshakeWait::Cancelled => return cancellation_exit(terminal_failure),
                HandshakeWait::Disconnected => {
                    return KmsRenderWorkerExit::RenderWorldHandoffAborted {
                        operation,
                        generation,
                        key,
                    };
                }
            };
            if quiescence_identity(&actual) != quiescence_identity(&expected)
                || actual.outcome != KmsRenderQuiescenceOutcome::Quiesced
            {
                return KmsRenderWorkerExit::RenderWorldHandoffAborted {
                    operation,
                    generation,
                    key,
                };
            }
        }
        if operation == KmsRenderOperation::Resume {
            control.lifecycle.begin_resuming();
        }
        let retirement = if matches!(
            operation,
            KmsRenderOperation::Suspend
                | KmsRenderOperation::ChangeOutput
                | KmsRenderOperation::RemoveOutput
        ) {
            platform.retire_submitted_work()
        } else {
            Ok(())
        };
        let outcome: Result<Option<KmsRenderWorkerEvent<T::Placeholder>>, _> = match retirement {
            Err(failure) => Err(failure),
            Ok(()) => match command {
                KmsRenderCommand::Suspend { generation } => platform.suspend().map(|()| {
                    Some(KmsRenderWorkerEvent::Reply(KmsRenderReply::Suspended {
                        generation,
                    }))
                }),
                KmsRenderCommand::Resume { generation } => {
                    platform.resume(generation).map(|()| None)
                }
                KmsRenderCommand::AddOutput { generation, output } => platform
                    .add_output(&output)
                    .map(|source| {
                        let source = instrument_frame_replies(
                            source,
                            generation,
                            output.key.clone(),
                            events.clone(),
                        );
                        KmsRenderWorkerEvent::SourceReady {
                            generation,
                            output,
                            source,
                        }
                    })
                    .map(Some),
                KmsRenderCommand::ChangeOutput { generation, output } => platform
                    .change_output(&output)
                    .map(|source| {
                        let source = instrument_frame_replies(
                            source,
                            generation,
                            output.key.clone(),
                            events.clone(),
                        );
                        KmsRenderWorkerEvent::SourceReady {
                            generation,
                            output,
                            source,
                        }
                    })
                    .map(Some),
                KmsRenderCommand::RemoveOutput { generation, key } => platform
                    .remove_output(&key)
                    .map(|()| {
                        KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputRemoved {
                            generation,
                            key,
                        })
                    })
                    .map(Some),
            },
        };

        let event = match outcome {
            Ok(Some(event)) => event,
            Ok(None) => {
                if operation == KmsRenderOperation::Resume {
                    control.lifecycle.active();
                }
                clear_worker_context(current);
                continue;
            }
            Err(failure) => {
                let failure = KmsRenderWorkerFailure {
                    operation,
                    generation,
                    key: key.clone(),
                    failure,
                };
                if !failure.failure.is_terminal()
                    && matches!(
                        operation,
                        KmsRenderOperation::AddOutput
                            | KmsRenderOperation::ChangeOutput
                            | KmsRenderOperation::RemoveOutput
                    )
                {
                    let reply = KmsRenderReply::OutputFailed {
                        generation,
                        key: key.expect("output operations carry a key"),
                        reason: format!("{}: {}", failure.failure.code, failure.failure.detail),
                    };
                    if events.send(KmsRenderWorkerEvent::Reply(reply)).is_err() {
                        return KmsRenderWorkerExit::ReplyChannelDisconnected {
                            operation,
                            generation,
                            key: failure.key,
                        };
                    }
                    clear_worker_context(current);
                    continue;
                }
                return KmsRenderWorkerExit::PlatformFailed(failure);
            }
        };
        match operation {
            KmsRenderOperation::Suspend => control.lifecycle.suspended(),
            KmsRenderOperation::Resume
            | KmsRenderOperation::ChangeOutput
            | KmsRenderOperation::RemoveOutput => control.lifecycle.active(),
            KmsRenderOperation::AddOutput | KmsRenderOperation::Worker => {}
        }
        let source_needs_registration = matches!(event, KmsRenderWorkerEvent::SourceReady { .. });
        if events.send(event).is_err() {
            if source_needs_registration {
                return registration_failure_exit(KmsRenderWorkerExit::ReplyChannelDisconnected {
                    operation,
                    generation,
                    key,
                });
            }
            return KmsRenderWorkerExit::ReplyChannelDisconnected {
                operation,
                generation,
                key,
            };
        }
        if source_needs_registration {
            let expected_key = key.clone().expect("source operations carry an output key");
            let registration = match wait_for_handshake(registrations) {
                HandshakeWait::Value(registration) => {
                    if cancelled.load(Ordering::Acquire) {
                        return registration_failure_exit(cancellation_exit(terminal_failure));
                    }
                    registration
                }
                HandshakeWait::Cancelled => {
                    return registration_failure_exit(cancellation_exit(terminal_failure));
                }
                HandshakeWait::Disconnected => {
                    return registration_failure_exit(
                        KmsRenderWorkerExit::RegistrationChannelDisconnected {
                            operation,
                            generation,
                            key: key.expect("source operations carry an output key"),
                        },
                    );
                }
            };
            if registration.generation != generation || registration.key != expected_key {
                return registration_failure_exit(
                    KmsRenderWorkerExit::UnexpectedRegistrationDisposition {
                        operation,
                        expected_generation: generation,
                        expected_key,
                        actual: registration,
                    },
                );
            }
            if registration.disposition == KmsRenderRegistrationDisposition::Rejected {
                return registration_failure_exit(KmsRenderWorkerExit::RenderWorldHandoffAborted {
                    operation,
                    generation,
                    key: Some(expected_key),
                });
            }
        }
        clear_worker_context(current);
    }
}

#[derive(Clone)]
struct KmsRenderWorkerContext {
    operation: KmsRenderOperation,
    generation: u64,
    key: Option<OutputKey>,
}

fn set_worker_context(
    current: &Mutex<Option<KmsRenderWorkerContext>>,
    operation: KmsRenderOperation,
    generation: u64,
    key: Option<OutputKey>,
) {
    *current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(KmsRenderWorkerContext {
        operation,
        generation,
        key,
    });
}

fn clear_worker_context(current: &Mutex<Option<KmsRenderWorkerContext>>) {
    *current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn panicked_worker_failure(
    current: &Mutex<Option<KmsRenderWorkerContext>>,
) -> KmsRenderWorkerFailure {
    let current = current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let (operation, generation, key) = current.map_or(
        (KmsRenderOperation::Worker, 0, None),
        |KmsRenderWorkerContext {
             operation,
             generation,
             key,
         }| (operation, generation, key),
    );
    KmsRenderWorkerFailure {
        operation,
        generation,
        key,
        failure: KmsRenderPlatformFailure::new(
            "render-worker-panicked",
            "KMS render worker panicked while an operation was in flight",
        ),
    }
}

fn finalize_worker_exit<P>(
    events: &Sender<KmsRenderWorkerEvent<P>>,
    commands: &Receiver<WorkerInbound<KmsRenderCommand>>,
    cancelled: &AtomicBool,
    ingress: &Mutex<()>,
    terminal_failure: &Mutex<Option<KmsRenderWorkerFailure>>,
    exit: &KmsRenderWorkerExit,
) {
    let _ingress = ingress
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(failure) = worker_failure_for_exit(exit) {
        latch_terminal_failure(terminal_failure, failure);
    }
    cancelled.store(true, Ordering::Release);
    publish_worker_exit(events, exit);
    publish_buffered_command_failures(events, commands, exit);
}

fn latch_terminal_failure(
    terminal_failure: &Mutex<Option<KmsRenderWorkerFailure>>,
    failure: KmsRenderWorkerFailure,
) {
    let mut terminal_failure = terminal_failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if terminal_failure.is_none() {
        *terminal_failure = Some(failure);
    }
}

fn cancellation_exit(
    terminal_failure: &Mutex<Option<KmsRenderWorkerFailure>>,
) -> KmsRenderWorkerExit {
    let terminal_failure = terminal_failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    terminal_failure
        .clone()
        .map_or(KmsRenderWorkerExit::Cancelled, |failure| {
            KmsRenderWorkerExit::RenderPathDisconnected(failure)
        })
}

enum WorkerExitReport {
    Silent,
    Stopped,
    Failed(KmsRenderWorkerFailure),
}

fn publish_worker_exit<P>(events: &Sender<KmsRenderWorkerEvent<P>>, exit: &KmsRenderWorkerExit) {
    match worker_exit_report(exit) {
        WorkerExitReport::Silent => {}
        WorkerExitReport::Stopped => {
            let _ = events.send(KmsRenderWorkerEvent::WorkerStopped(exit.clone()));
        }
        WorkerExitReport::Failed(failure) => {
            let _ = events.send(KmsRenderWorkerEvent::WorkerFailed(failure));
        }
    }
}

fn worker_failure_for_exit(exit: &KmsRenderWorkerExit) -> Option<KmsRenderWorkerFailure> {
    match worker_exit_report(exit) {
        WorkerExitReport::Failed(failure) => Some(failure),
        WorkerExitReport::Silent | WorkerExitReport::Stopped => None,
    }
}

fn worker_exit_report(exit: &KmsRenderWorkerExit) -> WorkerExitReport {
    match exit {
        KmsRenderWorkerExit::Panicked(failure)
        | KmsRenderWorkerExit::PlatformFailed(failure)
        | KmsRenderWorkerExit::RenderPathDisconnected(failure) => {
            WorkerExitReport::Failed(failure.clone())
        }
        KmsRenderWorkerExit::TeardownFailed { failure, .. } => {
            WorkerExitReport::Failed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::Worker,
                generation: 0,
                key: None,
                failure: failure.clone(),
            })
        }
        KmsRenderWorkerExit::RenderWorldDropUnproven { .. } => {
            WorkerExitReport::Failed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::Worker,
                generation: 0,
                key: None,
                failure: KmsRenderPlatformFailure::new(
                    "render-world-drop-unproven",
                    "render-world teardown acknowledgement was lost; live platform leaked fail-closed",
                ),
            })
        }
        KmsRenderWorkerExit::RegistrarChannelDisconnected {
            operation,
            generation,
            key,
        } => WorkerExitReport::Failed(KmsRenderWorkerFailure {
            operation: *operation,
            generation: *generation,
            key: key.clone(),
            failure: KmsRenderPlatformFailure::new(
                "registrar-release-channel-disconnected",
                "KMS registrar release channel disconnected while an operation was in flight",
            ),
        }),
        KmsRenderWorkerExit::UnexpectedRegistrarRelease { expected, actual } => {
            WorkerExitReport::Failed(KmsRenderWorkerFailure {
                operation: expected.operation,
                generation: expected.generation,
                key: expected.key.clone(),
                failure: KmsRenderPlatformFailure::new(
                    "unexpected-registrar-release",
                    format!("expected {expected:?}, received {actual:?}"),
                ),
            })
        }
        KmsRenderWorkerExit::RenderWorldHandoffAborted {
            operation,
            generation,
            key,
        } => WorkerExitReport::Failed(KmsRenderWorkerFailure {
            operation: *operation,
            generation: *generation,
            key: key.clone(),
            failure: KmsRenderPlatformFailure::new(
                "render-world-command-aborted",
                "KMS render-world command was dropped before the handoff completed",
            ),
        }),
        KmsRenderWorkerExit::RegistrationChannelDisconnected {
            operation,
            generation,
            key,
        } => WorkerExitReport::Failed(KmsRenderWorkerFailure {
            operation: *operation,
            generation: *generation,
            key: Some(key.clone()),
            failure: KmsRenderPlatformFailure::new(
                "registration-channel-disconnected",
                "KMS source-registration channel disconnected while an operation was in flight",
            ),
        }),
        KmsRenderWorkerExit::UnexpectedRegistrationDisposition {
            operation,
            expected_generation,
            expected_key,
            actual,
        } => WorkerExitReport::Failed(KmsRenderWorkerFailure {
            operation: *operation,
            generation: *expected_generation,
            key: Some(expected_key.clone()),
            failure: KmsRenderPlatformFailure::new(
                "unexpected-registration-disposition",
                format!(
                    "expected generation {expected_generation} for {expected_key:?}, received {actual:?}"
                ),
            ),
        }),
        KmsRenderWorkerExit::CommandChannelDisconnected => WorkerExitReport::Stopped,
        KmsRenderWorkerExit::Cancelled | KmsRenderWorkerExit::ReplyChannelDisconnected { .. } => {
            WorkerExitReport::Silent
        }
    }
}

fn publish_buffered_command_failures<P>(
    events: &Sender<KmsRenderWorkerEvent<P>>,
    commands: &Receiver<WorkerInbound<KmsRenderCommand>>,
    exit: &KmsRenderWorkerExit,
) {
    while let Ok(inbound) = commands.try_recv() {
        let WorkerInbound::Value(command) = inbound else {
            continue;
        };
        publish_stopped_command_failure(events, &command, exit);
    }
}

fn publish_stopped_command_failure<P>(
    events: &Sender<KmsRenderWorkerEvent<P>>,
    command: &KmsRenderCommand,
    exit: &KmsRenderWorkerExit,
) {
    let (operation, generation, key) = command_identity(command);
    let failure = KmsRenderWorkerFailure {
        operation,
        generation,
        key,
        failure: KmsRenderPlatformFailure::new(
            "render-worker-stopped-before-command",
            format!("KMS render worker stopped before accepting the command: {exit:?}"),
        ),
    };
    let _ = events.send(KmsRenderWorkerEvent::WorkerFailed(failure));
}

enum HandshakeWait<T> {
    Value(T),
    Cancelled,
    Disconnected,
}

fn wait_for_handshake<T>(receiver: &Receiver<WorkerInbound<T>>) -> HandshakeWait<T> {
    match receiver.recv() {
        Ok(WorkerInbound::Value(value)) => HandshakeWait::Value(value),
        Ok(WorkerInbound::Cancel) => HandshakeWait::Cancelled,
        Err(_) => HandshakeWait::Disconnected,
    }
}

fn release_identity(release: &KmsRenderRelease) -> (KmsRenderOperation, u64, Option<&OutputKey>) {
    (release.operation, release.generation, release.key.as_ref())
}

fn quiescence_identity(
    quiescence: &KmsRenderQuiescence,
) -> (KmsRenderOperation, u64, Option<&OutputKey>) {
    (
        quiescence.operation,
        quiescence.generation,
        quiescence.key.as_ref(),
    )
}

/// All six post-source registration failures are terminal. Cleanup is routed
/// through render-world drain plus the global drop acknowledgement; calling
/// `remove_output` here would destroy a live surface before either proof.
fn registration_failure_exit(exit: KmsRenderWorkerExit) -> KmsRenderWorkerExit {
    exit
}

fn instrument_frame_replies<P>(
    source: RenderSource<P>,
    generation: u64,
    key: OutputKey,
    events: Sender<KmsRenderWorkerEvent<P>>,
) -> RenderSource<P>
where
    P: PlaceholderExtent + Send + 'static,
{
    let RenderSource {
        placeholder,
        mut acquire,
    } = source;
    RenderSource {
        placeholder,
        acquire: Box::new(move || {
            let AcquiredOutputFrame { view, present } = acquire()?;
            let frame_events = events.clone();
            let frame_key = key.clone();
            Ok(AcquiredOutputFrame {
                view,
                present: fallible_present_output_frame(move |deadline| {
                    let outcome = present_output_frame(present, deadline)?;
                    if outcome == PresentOutcome::Displayed {
                        let _ = frame_events.send(KmsRenderWorkerEvent::Reply(
                            KmsRenderReply::FrameSubmitted {
                                generation,
                                key: frame_key,
                            },
                        ));
                    }
                    Ok(outcome)
                }),
            })
        }),
    }
}

fn command_identity(command: &KmsRenderCommand) -> (KmsRenderOperation, u64, Option<OutputKey>) {
    match command {
        KmsRenderCommand::Suspend { generation } => {
            (KmsRenderOperation::Suspend, *generation, None)
        }
        KmsRenderCommand::Resume { generation } => (KmsRenderOperation::Resume, *generation, None),
        KmsRenderCommand::AddOutput { generation, output } => (
            KmsRenderOperation::AddOutput,
            *generation,
            Some(output.key.clone()),
        ),
        KmsRenderCommand::ChangeOutput { generation, output } => (
            KmsRenderOperation::ChangeOutput,
            *generation,
            Some(output.key.clone()),
        ),
        KmsRenderCommand::RemoveOutput { generation, key } => (
            KmsRenderOperation::RemoveOutput,
            *generation,
            Some(key.clone()),
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedOutputPhase {
    Adding,
    Changing,
    Removing,
}

#[derive(Clone, Debug)]
struct ExpectedOutput {
    generation: u64,
    phase: ExpectedOutputPhase,
    output: Option<SelectedOutput>,
}

pub(crate) struct RegisteredRenderSource {
    pub(crate) generation: u64,
    pub(crate) handle: ManualTextureViewHandle,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RegistrarIgnoreReason {
    NoExpectedTransition,
    SupersededGeneration { expected: u64 },
    UnexpectedTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegistrarIgnoredEvent {
    pub(crate) generation: u64,
    pub(crate) key: Option<OutputKey>,
    pub(crate) reason: RegistrarIgnoreReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RenderSourceRegistrarError {
    PlaceholderSizeMismatch {
        generation: u64,
        key: OutputKey,
        expected: (u32, u32),
        actual: (u32, u32),
    },
    HandleExhausted {
        generation: u64,
        key: OutputKey,
    },
}

impl RenderSourceRegistrarError {
    pub(crate) fn failure_reply(&self) -> KmsRenderReply {
        match self {
            Self::PlaceholderSizeMismatch {
                generation,
                key,
                expected,
                actual,
            } => KmsRenderReply::OutputFailed {
                generation: *generation,
                key: key.clone(),
                reason: format!(
                    "placeholder-size-mismatch: expected {}x{}, got {}x{}",
                    expected.0, expected.1, actual.0, actual.1
                ),
            },
            Self::HandleExhausted { generation, key } => KmsRenderReply::OutputFailed {
                generation: *generation,
                key: key.clone(),
                reason: "manual-texture-view-handle-exhausted".into(),
            },
        }
    }

    pub(crate) fn rejected_registration(&self) -> KmsRenderRegistration {
        let (generation, key) = match self {
            Self::PlaceholderSizeMismatch {
                generation, key, ..
            }
            | Self::HandleExhausted { generation, key } => (*generation, key.clone()),
        };
        KmsRenderRegistration {
            generation,
            key,
            disposition: KmsRenderRegistrationDisposition::Rejected,
        }
    }
}

pub(crate) enum RegistrarEffect<P> {
    Install {
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
        handle: ManualTextureViewHandle,
        placeholder: P,
        acquire: Box<
            dyn FnMut() -> Result<AcquiredOutputFrame, KmsRenderPlatformFailure>
                + Send
                + Sync
                + 'static,
        >,
    },
    Deactivate {
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
        handle: Option<ManualTextureViewHandle>,
    },
    Remove {
        operation: KmsRenderOperation,
        generation: u64,
        key: OutputKey,
        handle: ManualTextureViewHandle,
    },
    Clear {
        generation: u64,
    },
    Terminate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RegistrarIdentity {
    pub(crate) operation: KmsRenderOperation,
    pub(crate) generation: u64,
    pub(crate) key: Option<OutputKey>,
}

pub(crate) struct RegistrarUpdate<P> {
    pub(crate) identity: Option<RegistrarIdentity>,
    pub(crate) reply: Option<KmsRenderReply>,
    pub(crate) effects: Vec<RegistrarEffect<P>>,
    pub(crate) registration: Option<KmsRenderRegistration>,
}

pub(crate) struct RenderSourceRegistrar<P> {
    active: bool,
    terminal: bool,
    session_generation: u64,
    next_handle: u32,
    expected: BTreeMap<OutputKey, ExpectedOutput>,
    registered: BTreeMap<OutputKey, RegisteredRenderSource>,
    ignored_events: u64,
    last_ignored_event: Option<RegistrarIgnoredEvent>,
    placeholder: PhantomData<P>,
}

/// Handle zero is the unallocated sentinel. Keep every registrar-owned manual
/// texture-view handle in the positive range so a default value cannot alias it.
pub(crate) const FIRST_REGISTRAR_MANUAL_TEXTURE_VIEW_HANDLE: u32 = 1;

impl<P> Default for RenderSourceRegistrar<P> {
    fn default() -> Self {
        Self {
            active: true,
            terminal: false,
            session_generation: 0,
            // KmsRenderTargetPlugin installs the process's sole registrar;
            // every presented-output insertion comes from this positive range.
            next_handle: FIRST_REGISTRAR_MANUAL_TEXTURE_VIEW_HANDLE,
            expected: BTreeMap::new(),
            registered: BTreeMap::new(),
            ignored_events: 0,
            last_ignored_event: None,
            placeholder: PhantomData,
        }
    }
}

impl<P> RenderSourceRegistrar<P>
where
    P: PlaceholderExtent,
{
    pub(crate) fn apply(
        &mut self,
        event: KmsRenderWorkerEvent<P>,
    ) -> Result<RegistrarUpdate<P>, RenderSourceRegistrarError> {
        match event {
            KmsRenderWorkerEvent::WorkerFailed(failure) => {
                let identity = RegistrarIdentity {
                    operation: failure.operation,
                    generation: failure.generation,
                    key: failure.key.clone(),
                };
                let effects = self.transition_terminal().into_iter().collect();
                Ok(RegistrarUpdate {
                    identity: Some(identity),
                    reply: Some(KmsRenderReply::WorkerFailed {
                        operation: failure.operation,
                        generation: failure.generation,
                        key: failure.key,
                        code: failure.failure.code,
                        reason: failure.failure.detail,
                    }),
                    effects,
                    registration: None,
                })
            }
            KmsRenderWorkerEvent::WorkerStopped(_) => {
                let effects = self.transition_terminal().into_iter().collect();
                Ok(RegistrarUpdate {
                    identity: None,
                    reply: None,
                    effects,
                    registration: None,
                })
            }
            _ if self.terminal => Ok(empty_update()),
            KmsRenderWorkerEvent::CommandAccepted(command) => Ok(self.accept_command(command)),
            KmsRenderWorkerEvent::SourceReady {
                generation,
                output,
                source,
            } => self.source_ready(generation, output, source),
            KmsRenderWorkerEvent::Reply(reply) => Ok(self.apply_reply(reply)),
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub(super) fn transition_terminal(&mut self) -> Option<RegistrarEffect<P>> {
        let was_terminal = self.terminal;
        self.terminal = true;
        self.expected.clear();
        self.registered.clear();
        (!was_terminal).then_some(RegistrarEffect::Terminate)
    }

    pub(super) fn rollback_registration(
        &mut self,
        registration: &KmsRenderRegistration,
    ) -> Option<ManualTextureViewHandle> {
        if registration.disposition != KmsRenderRegistrationDisposition::Accepted {
            return None;
        }
        if self
            .expected
            .get(&registration.key)
            .is_some_and(|expected| expected.generation == registration.generation)
        {
            self.expected.remove(&registration.key);
        }
        if self
            .registered
            .get(&registration.key)
            .is_some_and(|registered| registered.generation == registration.generation)
        {
            return self
                .registered
                .remove(&registration.key)
                .map(|registered| registered.handle);
        }
        None
    }

    pub(super) fn expected_operation(
        &self,
        generation: u64,
        key: &OutputKey,
    ) -> Option<KmsRenderOperation> {
        let expected = self.expected.get(key)?;
        if expected.generation != generation {
            return None;
        }
        Some(match expected.phase {
            ExpectedOutputPhase::Adding => KmsRenderOperation::AddOutput,
            ExpectedOutputPhase::Changing => KmsRenderOperation::ChangeOutput,
            ExpectedOutputPhase::Removing => KmsRenderOperation::RemoveOutput,
        })
    }

    #[cfg(test)]
    pub(crate) fn registered(&self, key: &OutputKey) -> Option<&RegisteredRenderSource> {
        self.registered.get(key)
    }

    #[cfg(test)]
    pub(crate) fn expected_generation(&self, key: &OutputKey) -> Option<u64> {
        self.expected.get(key).map(|expected| expected.generation)
    }

    #[cfg(test)]
    pub(crate) fn ignored_events(&self) -> u64 {
        self.ignored_events
    }

    fn accept_command(&mut self, command: KmsRenderCommand) -> RegistrarUpdate<P> {
        let (operation, generation, key) = command_identity(&command);
        let mut effects = Vec::new();
        match command {
            KmsRenderCommand::Suspend { generation } => {
                self.active = false;
                self.session_generation = generation;
                self.expected.clear();
                self.registered.clear();
                effects.push(RegistrarEffect::Clear { generation });
            }
            KmsRenderCommand::Resume { generation } => {
                self.active = true;
                self.session_generation = generation;
            }
            KmsRenderCommand::AddOutput { generation, output } => {
                self.expect_output(generation, ExpectedOutputPhase::Adding, output);
            }
            KmsRenderCommand::ChangeOutput { generation, output } => {
                let handle = self.registered.get_mut(&output.key).map(|registered| {
                    registered.active = false;
                    registered.handle
                });
                effects.push(RegistrarEffect::Deactivate {
                    operation: KmsRenderOperation::ChangeOutput,
                    generation,
                    key: output.key.clone(),
                    handle,
                });
                self.expect_output(generation, ExpectedOutputPhase::Changing, output);
            }
            KmsRenderCommand::RemoveOutput { generation, key } => {
                let handle = self.registered.get_mut(&key).map(|registered| {
                    registered.active = false;
                    registered.handle
                });
                effects.push(RegistrarEffect::Deactivate {
                    operation: KmsRenderOperation::RemoveOutput,
                    generation,
                    key: key.clone(),
                    handle,
                });
                self.expected.insert(
                    key,
                    ExpectedOutput {
                        generation,
                        phase: ExpectedOutputPhase::Removing,
                        output: None,
                    },
                );
            }
        }
        RegistrarUpdate {
            identity: Some(RegistrarIdentity {
                operation,
                generation,
                key,
            }),
            reply: None,
            effects,
            registration: None,
        }
    }

    fn expect_output(
        &mut self,
        generation: u64,
        phase: ExpectedOutputPhase,
        output: SelectedOutput,
    ) {
        self.expected.insert(
            output.key.clone(),
            ExpectedOutput {
                generation,
                phase,
                output: Some(output),
            },
        );
    }

    fn source_ready(
        &mut self,
        generation: u64,
        output: SelectedOutput,
        source: RenderSource<P>,
    ) -> Result<RegistrarUpdate<P>, RenderSourceRegistrarError> {
        let key = output.key.clone();
        if !self.active {
            self.record_ignored(
                generation,
                Some(key.clone()),
                RegistrarIgnoreReason::UnexpectedTransition,
            );
            return Ok(rejected_source_update(generation, key));
        }
        let Some(expected) = self.expected.get(&key) else {
            self.record_ignored(
                generation,
                Some(key.clone()),
                RegistrarIgnoreReason::NoExpectedTransition,
            );
            return Ok(rejected_source_update(generation, key));
        };
        if expected.generation != generation {
            let expected_generation = expected.generation;
            self.record_ignored(
                generation,
                Some(key.clone()),
                RegistrarIgnoreReason::SupersededGeneration {
                    expected: expected_generation,
                },
            );
            return Ok(rejected_source_update(generation, key));
        }
        if !matches!(
            expected.phase,
            ExpectedOutputPhase::Adding | ExpectedOutputPhase::Changing
        ) || expected.output.as_ref() != Some(&output)
        {
            self.record_ignored(
                generation,
                Some(key.clone()),
                RegistrarIgnoreReason::UnexpectedTransition,
            );
            return Ok(rejected_source_update(generation, key));
        }
        let expected_size = (output.display.mode.width, output.display.mode.height);
        let actual_size = source.placeholder.extent();
        if actual_size != expected_size {
            return Err(RenderSourceRegistrarError::PlaceholderSizeMismatch {
                generation,
                key,
                expected: expected_size,
                actual: actual_size,
            });
        }
        let expected_phase = expected.phase;

        let handle = if let Some(registered) = self.registered.get(&output.key) {
            registered.handle
        } else {
            self.allocate_handle(generation, &output.key)?
        };
        let effect_key = output.key.clone();
        let operation = match expected_phase {
            ExpectedOutputPhase::Adding => KmsRenderOperation::AddOutput,
            ExpectedOutputPhase::Changing => KmsRenderOperation::ChangeOutput,
            ExpectedOutputPhase::Removing => unreachable!("a removal cannot produce a source"),
        };
        self.registered.insert(
            output.key.clone(),
            RegisteredRenderSource {
                generation,
                handle,
                active: self.active,
            },
        );
        Ok(RegistrarUpdate {
            identity: Some(RegistrarIdentity {
                operation,
                generation,
                key: Some(output.key.clone()),
            }),
            reply: Some(KmsRenderReply::OutputReady {
                generation,
                key: effect_key.clone(),
            }),
            effects: vec![RegistrarEffect::Install {
                operation,
                generation,
                key: effect_key,
                handle,
                placeholder: source.placeholder,
                acquire: source.acquire,
            }],
            registration: Some(KmsRenderRegistration {
                generation,
                key: output.key,
                disposition: KmsRenderRegistrationDisposition::Accepted,
            }),
        })
    }

    fn apply_reply(&mut self, reply: KmsRenderReply) -> RegistrarUpdate<P> {
        match reply.clone() {
            KmsRenderReply::Suspended { generation } => {
                if self.active || self.session_generation != generation {
                    self.record_session_ignored(generation);
                    return empty_update();
                }
                self.expected.clear();
                self.registered.clear();
                RegistrarUpdate {
                    identity: Some(RegistrarIdentity {
                        operation: KmsRenderOperation::Suspend,
                        generation,
                        key: None,
                    }),
                    reply: Some(reply),
                    effects: Vec::new(),
                    registration: None,
                }
            }
            KmsRenderReply::OutputRemoved { generation, key } => {
                if !self.expected_matches(&key, generation, ExpectedOutputPhase::Removing) {
                    self.record_output_ignored(generation, key);
                    return empty_update();
                }
                self.expected.remove(&key);
                let effects = self
                    .registered
                    .remove(&key)
                    .map(|registered| {
                        vec![RegistrarEffect::Remove {
                            operation: KmsRenderOperation::RemoveOutput,
                            generation,
                            key: key.clone(),
                            handle: registered.handle,
                        }]
                    })
                    .unwrap_or_default();
                RegistrarUpdate {
                    identity: Some(RegistrarIdentity {
                        operation: KmsRenderOperation::RemoveOutput,
                        generation,
                        key: Some(key.clone()),
                    }),
                    reply: Some(reply),
                    effects,
                    registration: None,
                }
            }
            KmsRenderReply::OutputFailed {
                generation,
                ref key,
                ..
            } => {
                let Some(expected) = self.expected.get(key) else {
                    self.record_output_ignored(generation, key.clone());
                    return empty_update();
                };
                if expected.generation != generation {
                    self.record_output_ignored(generation, key.clone());
                    return empty_update();
                }
                let removing = expected.phase == ExpectedOutputPhase::Removing;
                let operation = match expected.phase {
                    ExpectedOutputPhase::Adding => KmsRenderOperation::AddOutput,
                    ExpectedOutputPhase::Changing => KmsRenderOperation::ChangeOutput,
                    ExpectedOutputPhase::Removing => KmsRenderOperation::RemoveOutput,
                };
                self.expected.remove(key);
                let effects = if removing {
                    Vec::new()
                } else {
                    self.registered
                        .remove(key)
                        .map(|registered| {
                            vec![RegistrarEffect::Remove {
                                operation,
                                generation,
                                key: key.clone(),
                                handle: registered.handle,
                            }]
                        })
                        .unwrap_or_default()
                };
                RegistrarUpdate {
                    identity: Some(RegistrarIdentity {
                        operation,
                        generation,
                        key: Some(key.clone()),
                    }),
                    reply: Some(reply),
                    effects,
                    registration: None,
                }
            }
            KmsRenderReply::FrameSubmitted {
                generation,
                ref key,
            } => {
                if self.registered.get(key).is_none_or(|registered| {
                    registered.generation != generation || !registered.active
                }) {
                    self.record_output_ignored(generation, key.clone());
                    return empty_update();
                }
                RegistrarUpdate {
                    identity: Some(RegistrarIdentity {
                        operation: KmsRenderOperation::Worker,
                        generation,
                        key: Some(key.clone()),
                    }),
                    reply: Some(reply),
                    effects: Vec::new(),
                    registration: None,
                }
            }
            KmsRenderReply::OutputReady { generation, key } => {
                self.record_output_ignored(generation, key);
                empty_update()
            }
            KmsRenderReply::WorkerFailed {
                operation,
                generation,
                ref key,
                ..
            } => RegistrarUpdate {
                identity: Some(RegistrarIdentity {
                    operation,
                    generation,
                    key: key.clone(),
                }),
                reply: Some(reply),
                effects: Vec::new(),
                registration: None,
            },
        }
    }

    fn expected_matches(
        &self,
        key: &OutputKey,
        generation: u64,
        phase: ExpectedOutputPhase,
    ) -> bool {
        self.expected
            .get(key)
            .is_some_and(|expected| expected.generation == generation && expected.phase == phase)
    }

    fn allocate_handle(
        &mut self,
        generation: u64,
        key: &OutputKey,
    ) -> Result<ManualTextureViewHandle, RenderSourceRegistrarError> {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or_else(|| {
            RenderSourceRegistrarError::HandleExhausted {
                generation,
                key: key.clone(),
            }
        })?;
        Ok(ManualTextureViewHandle(handle))
    }

    fn record_session_ignored(&mut self, generation: u64) {
        let reason = if self.session_generation == 0 {
            RegistrarIgnoreReason::NoExpectedTransition
        } else {
            RegistrarIgnoreReason::SupersededGeneration {
                expected: self.session_generation,
            }
        };
        self.record_ignored(generation, None, reason);
    }

    fn record_output_ignored(&mut self, generation: u64, key: OutputKey) {
        let reason = self
            .expected
            .get(&key)
            .map(|expected| RegistrarIgnoreReason::SupersededGeneration {
                expected: expected.generation,
            })
            .unwrap_or(RegistrarIgnoreReason::NoExpectedTransition);
        self.record_ignored(generation, Some(key), reason);
    }

    fn record_ignored(
        &mut self,
        generation: u64,
        key: Option<OutputKey>,
        reason: RegistrarIgnoreReason,
    ) {
        self.ignored_events = self.ignored_events.saturating_add(1);
        self.last_ignored_event = Some(RegistrarIgnoredEvent {
            generation,
            key,
            reason,
        });
    }
}

fn empty_update<P>() -> RegistrarUpdate<P> {
    RegistrarUpdate {
        identity: None,
        reply: None,
        effects: Vec::new(),
        registration: None,
    }
}

fn rejected_source_update<P>(generation: u64, key: OutputKey) -> RegistrarUpdate<P> {
    RegistrarUpdate {
        identity: Some(RegistrarIdentity {
            operation: KmsRenderOperation::Worker,
            generation,
            key: Some(key.clone()),
        }),
        reply: None,
        effects: Vec::new(),
        registration: Some(KmsRenderRegistration {
            generation,
            key,
            disposition: KmsRenderRegistrationDisposition::Rejected,
        }),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    use super::*;
    use crate::backend::kms::{AtomicOutputSelection, ConnectorMode, LogicalRect};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakePlaceholder((u32, u32));

    impl PlaceholderExtent for FakePlaceholder {
        fn extent(&self) -> (u32, u32) {
            self.0
        }
    }

    fn noop_manual_view(width: u32, height: u32, label: &'static str) -> ManualTextureView {
        let (device, _queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        ManualTextureView::with_default_format(
            texture
                .create_view(&wgpu::TextureViewDescriptor::default())
                .into(),
            bevy::prelude::UVec2::new(width, height),
        )
    }

    #[test]
    fn frame_submitted_is_emitted_only_for_displayed_outcome() {
        let key = OutputKey {
            device: 226,
            connector_name: "Test-1".into(),
        };

        for (outcome, expected_submission_count) in [
            (PresentOutcome::Cancelled, 0),
            (PresentOutcome::Displayed, 1),
        ] {
            let view = noop_manual_view(16, 16, "instrumented frame reply");
            let source = RenderSource {
                placeholder: FakePlaceholder((16, 16)),
                acquire: Box::new(move || {
                    Ok(AcquiredOutputFrame {
                        view: view.clone(),
                        present: fallible_present_output_frame(move |_| Ok(outcome)),
                    })
                }),
            };
            let (events, received) = mpsc::channel();
            let mut source = instrument_frame_replies(source, 41, key.clone(), events);
            let frame = (source.acquire)().expect("fake source acquires");

            assert_eq!(
                present_output_frame(frame.present, PresentDeadline::unbounded_non_presenting()),
                Ok(outcome)
            );
            let replies = received.try_iter().collect::<Vec<_>>();
            assert_eq!(replies.len(), expected_submission_count);
            if outcome == PresentOutcome::Displayed {
                assert!(matches!(
                    replies.as_slice(),
                    [KmsRenderWorkerEvent::Reply(KmsRenderReply::FrameSubmitted {
                        generation: 41,
                        key: submitted_key,
                    })] if *submitted_key == key
                ));
            }
        }
    }

    #[test]
    fn atomic_commit_authority_classification_is_exact() {
        for errno in [libc::EACCES, libc::EPERM, libc::ENODEV] {
            let failure = KmsRenderPlatformFailure::terminal(
                "kms-live-atomic-commit-hard-rejection",
                format!("atomic commit ioctl failed with errno {errno}: injected"),
            );
            assert_eq!(failure.atomic_commit_authority_errno(), Some(errno));
        }

        let invalid = KmsRenderPlatformFailure::terminal(
            "kms-live-atomic-commit-hard-rejection",
            format!(
                "atomic commit ioctl failed with errno {}: injected",
                libc::EINVAL
            ),
        );
        assert_eq!(invalid.atomic_commit_authority_errno(), None);

        let foreign = KmsRenderPlatformFailure::terminal(
            "kms-live-unrelated-failure",
            format!(
                "atomic commit ioctl failed with errno {}: injected",
                libc::EACCES
            ),
        );
        assert_eq!(foreign.atomic_commit_authority_errno(), None);

        let recoverable = KmsRenderPlatformFailure::new(
            "kms-live-atomic-commit-hard-rejection",
            format!(
                "atomic commit ioctl failed with errno {}: injected",
                libc::EACCES
            ),
        );
        assert_eq!(recoverable.atomic_commit_authority_errno(), None);
    }

    #[derive(Clone, Default)]
    struct Barrier {
        state: Arc<(Mutex<BarrierState>, Condvar)>,
    }

    type OperationLog = Arc<Mutex<Vec<(KmsRenderOperation, Option<u32>)>>>;

    #[derive(Default)]
    struct BarrierState {
        entered: bool,
        released: bool,
    }

    impl Barrier {
        fn enter_and_wait(&self) {
            let (lock, condition) = &*self.state;
            let mut state = lock.lock().expect("barrier state");
            state.entered = true;
            condition.notify_all();
            while !state.released {
                state = condition.wait(state).expect("barrier wait");
            }
        }

        fn wait_until_entered(&self) {
            let (lock, condition) = &*self.state;
            let mut state = lock.lock().expect("barrier state");
            let deadline = Instant::now() + Duration::from_secs(30);
            while !state.entered {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .expect("fake platform did not enter the blocking operation before deadline");
                let (next, timeout) = condition
                    .wait_timeout(state, remaining)
                    .expect("barrier wait");
                state = next;
                assert!(
                    state.entered || !timeout.timed_out(),
                    "fake platform did not enter the blocking operation before deadline"
                );
            }
        }

        fn release(&self) {
            let (lock, condition) = &*self.state;
            let mut state = lock.lock().expect("barrier state");
            state.released = true;
            condition.notify_all();
        }
    }

    struct FakePlatform {
        operations: OperationLog,
        add_barrier: Option<Barrier>,
        acquire_barrier: Option<Barrier>,
        fail_remove: bool,
    }

    struct BlockingResumePlatform {
        operations: OperationLog,
        resume_barrier: Barrier,
    }

    enum GuardedExitMode {
        Panic,
        Fail,
        Add(Barrier),
        Teardown(Barrier),
        TeardownFail,
    }

    struct GuardedExitPlatform {
        mode: GuardedExitMode,
        teardown: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for GuardedExitPlatform {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl KmsRenderPlatform for GuardedExitPlatform {
        type Placeholder = FakePlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            match self.mode {
                GuardedExitMode::Panic => panic!("injected guarded worker panic"),
                GuardedExitMode::Fail => Err(KmsRenderPlatformFailure::new(
                    "injected-terminal-failure",
                    "guarded platform failure",
                )),
                GuardedExitMode::Add(_)
                | GuardedExitMode::Teardown(_)
                | GuardedExitMode::TeardownFail => Ok(()),
            }
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            if let GuardedExitMode::Add(barrier) = &self.mode {
                barrier.enter_and_wait();
            }
            Ok(FakePlatform::source(output, None))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn teardown(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            if let GuardedExitMode::Teardown(barrier) = &self.mode {
                barrier.enter_and_wait();
            }
            self.teardown.store(true, Ordering::SeqCst);
            if matches!(self.mode, GuardedExitMode::TeardownFail) {
                Err(KmsRenderPlatformFailure::new(
                    "injected-teardown-failure",
                    "guarded teardown failed",
                ))
            } else {
                Ok(())
            }
        }
    }

    impl FakePlatform {
        fn source(
            output: &SelectedOutput,
            acquire_barrier: Option<Barrier>,
        ) -> RenderSource<FakePlaceholder> {
            RenderSource {
                placeholder: FakePlaceholder((
                    output.display.mode.width,
                    output.display.mode.height,
                )),
                acquire: Box::new(move || {
                    if let Some(barrier) = &acquire_barrier {
                        barrier.enter_and_wait();
                    }
                    Err("fake frame unavailable".into())
                }),
            }
        }

        fn record(&self, operation: KmsRenderOperation, connector_id: Option<u32>) {
            assert_eq!(thread::current().name(), Some("cosmix-kms-render"));
            self.operations
                .lock()
                .expect("fake operation log")
                .push((operation, connector_id));
        }
    }

    impl KmsRenderPlatform for FakePlatform {
        type Placeholder = FakePlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Suspend, None);
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Resume, None);
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::AddOutput, Some(output.connector_id));
            if let Some(barrier) = &self.add_barrier {
                barrier.enter_and_wait();
            }
            Ok(Self::source(output, self.acquire_barrier.clone()))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::ChangeOutput, Some(output.connector_id));
            Ok(Self::source(output, self.acquire_barrier.clone()))
        }

        fn remove_output(&mut self, key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::RemoveOutput, None);
            if self.fail_remove {
                return Err(KmsRenderPlatformFailure::new(
                    "fake-remove-refused",
                    key.connector_name.clone(),
                ));
            }
            Ok(())
        }
    }

    impl BlockingResumePlatform {
        fn record(&self, operation: KmsRenderOperation, connector_id: Option<u32>) {
            self.operations
                .lock()
                .expect("blocking resume operation log")
                .push((operation, connector_id));
        }
    }

    impl KmsRenderPlatform for BlockingResumePlatform {
        type Placeholder = FakePlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Suspend, None);
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::Resume, None);
            self.resume_barrier.enter_and_wait();
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::AddOutput, Some(output.connector_id));
            Ok(FakePlatform::source(output, None))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::ChangeOutput, Some(output.connector_id));
            Ok(FakePlatform::source(output, None))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            self.record(KmsRenderOperation::RemoveOutput, None);
            Ok(())
        }
    }

    fn output(connector_id: u32, width: u32) -> SelectedOutput {
        let key = OutputKey {
            device: 226,
            connector_name: format!("Virtual-{connector_id}"),
        };
        let connector_mode = ConnectorMode {
            width,
            height: 720,
            refresh_millihz: 60_000,
            preferred: true,
            clock_khz: 74_250,
            hsync: (1390, 1430, 1650),
            vsync: (725, 730, 750),
            hskew: 0,
            vscan: 0,
            flags: 0,
        };
        SelectedOutput {
            key,
            connector_id,
            connector_mode,
            display: AtomicOutputSelection {
                connector_id,
                crtc_id: connector_id.saturating_add(100),
                primary_plane_id: connector_id.saturating_add(200),
                mode: connector_mode,
                format: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
            },
            output_scale: crate::backend::kms::OutputScale120::ONE,
            logical_rect: LogicalRect {
                x: 0,
                y: 0,
                width: width as i32,
                height: 720,
            },
        }
    }

    fn guarded_platform(
        mode: GuardedExitMode,
    ) -> (GuardedExitPlatform, Arc<AtomicBool>, Arc<AtomicBool>) {
        let teardown = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        (
            GuardedExitPlatform {
                mode,
                teardown: Arc::clone(&teardown),
                dropped: Arc::clone(&dropped),
            },
            teardown,
            dropped,
        )
    }

    #[test]
    fn lifecycle_reserves_rung_f_suspend_resume_without_weakening_terminal_states() {
        let lifecycle = KmsRenderLifecycle::new();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Active);
        lifecycle.begin_quiescing();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Quiescing);
        lifecycle.suspended();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Suspended);
        lifecycle.begin_resuming();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Resuming);
        lifecycle.active();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Active);
        lifecycle.begin_termination();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Terminating);
        lifecycle.begin_quiescing();
        lifecycle.suspended();
        lifecycle.begin_resuming();
        lifecycle.active();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Terminating);
        lifecycle.terminated();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Terminated);
        lifecycle.begin_quiescing();
        lifecycle.suspended();
        lifecycle.begin_resuming();
        lifecycle.active();
        lifecycle.begin_termination();
        assert_eq!(lifecycle.state(), KmsRenderLifecycleState::Terminated);
    }

    #[test]
    fn guarded_panic_retains_platform_until_render_world_drop() {
        let (platform, teardown, dropped) = guarded_platform(GuardedExitMode::Panic);
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 1 })
            .expect("queue panic");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(_))
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                failure: KmsRenderPlatformFailure {
                    code: "render-worker-panicked",
                    ..
                },
                ..
            }))
        ));
        assert!(!teardown.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        render_world_dropped.acknowledge();
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Panicked(_))
        ));
        assert!(teardown.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn guarded_terminal_failure_retains_platform_until_render_world_drop() {
        let (platform, teardown, dropped) = guarded_platform(GuardedExitMode::Fail);
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 2 })
            .expect("queue failure");
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(_))
        ));
        assert!(!teardown.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        render_world_dropped.acknowledge();
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::PlatformFailed(_))
        ));
        assert!(teardown.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn guarded_teardown_failure_is_returned_in_the_worker_exit() {
        let (platform, teardown, dropped) = guarded_platform(GuardedExitMode::TeardownFail);
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 22 })
            .expect("queue successful operation");
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        render_world_dropped.acknowledge();
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::TeardownFailed {
                prior,
                failure: KmsRenderPlatformFailure {
                    code: "injected-teardown-failure",
                    ..
                },
            }) if *prior == KmsRenderWorkerExit::Cancelled
        ));
        assert!(teardown.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn lost_render_world_acknowledgement_leaks_platform_fail_closed() {
        let (platform, teardown, dropped) = guarded_platform(GuardedExitMode::Fail);
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 20 })
            .expect("queue failure");
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        drop(render_world_dropped);
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderWorldDropUnproven {
                prior,
            }) if matches!(*prior, KmsRenderWorkerExit::PlatformFailed(_))
        ));
        assert!(!teardown.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn guarded_driver_teardown_detaches_after_deadline_without_dropping_platform() {
        let barrier = Barrier::default();
        let (platform, teardown, dropped) =
            guarded_platform(GuardedExitMode::Teardown(barrier.clone()));
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 21 })
            .expect("queue successful operation");
        let _ = receiver.recv_timeout(Duration::from_secs(2));
        render_world_dropped.acknowledge();
        let (finish_sender, finish_receiver) = mpsc::sync_channel(1);
        let finish_thread = thread::spawn(move || {
            let _ = finish_sender.send(worker.finish(Duration::from_millis(10)));
        });
        assert_eq!(
            finish_receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("guarded finish returns before the test deadline"),
            KmsRenderJoinOutcome::TimedOut
        );
        finish_thread.join().expect("guarded finish caller exits");
        barrier.wait_until_entered();
        assert!(!teardown.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        barrier.release();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(teardown.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn guarded_disconnected_render_path_retains_platform_until_render_world_drop() {
        let barrier = Barrier::default();
        let (platform, teardown, dropped) = guarded_platform(GuardedExitMode::Add(barrier.clone()));
        let (events, receiver) = mpsc::channel();
        let (worker, render_world_dropped) =
            KmsRenderWorker::spawn_guarded(platform, events).expect("guarded worker starts");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 3,
                output: output(3, 1280),
            })
            .expect("queue source");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(_))
        ));
        barrier.wait_until_entered();
        drop(receiver);
        barrier.release();
        assert!(!teardown.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        render_world_dropped.acknowledge();
        assert!(matches!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::ReplyChannelDisconnected { .. })
        ));
        assert!(teardown.load(Ordering::SeqCst));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn worker_is_named_and_processes_every_command_without_coalescing() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let add_barrier = Barrier::default();
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: Some(add_barrier.clone()),
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let quiescences = worker.quiescence_sender();
        let registrations = worker.registration_sender();
        let first = output(31, 1280);
        let mut changed = output(44, 1920);
        changed.key = first.key.clone();
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 1,
                output: first.clone(),
            })
            .expect("queue add");
        add_barrier.wait_until_entered();
        worker
            .send(KmsRenderCommand::ChangeOutput {
                generation: 2,
                output: changed.clone(),
            })
            .expect("queue change");
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 3,
                key: changed.key.clone(),
            })
            .expect("queue remove");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::ChangeOutput,
                generation: 2,
                key: Some(changed.key.clone()),
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("release changed source");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 3,
                key: Some(changed.key.clone()),
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("release removed source");
        quiescences
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::ChangeOutput,
                generation: 2,
                key: Some(changed.key.clone()),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("changed source quiesced");
        quiescences
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 3,
                key: Some(changed.key.clone()),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("removed source quiesced");
        add_barrier.release();

        let mut registrar = RenderSourceRegistrar::<FakePlaceholder>::default();
        let mut replies = Vec::new();
        for _ in 0..6 {
            let event = receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("worker event before deadline");
            let update = registrar.apply(event).expect("registrar transition");
            if let Some(registration) = update.registration {
                registrations
                    .send(registration)
                    .expect("return registration disposition");
            }
            if let Some(reply) = update.reply {
                replies.push(reply);
            }
        }
        assert!(
            matches!(
                replies.as_slice(),
                [
                    KmsRenderReply::OutputReady { generation: 1, .. },
                    KmsRenderReply::OutputReady { generation: 2, .. },
                    KmsRenderReply::OutputRemoved { generation: 3, .. },
                ]
            ),
            "unexpected registrar replies: {replies:?}"
        );
        assert_eq!(registrar.ignored_events(), 0);
        assert_eq!(
            *operations.lock().expect("operation log"),
            vec![
                (KmsRenderOperation::AddOutput, Some(31)),
                (KmsRenderOperation::ChangeOutput, Some(44)),
                (KmsRenderOperation::RemoveOutput, None),
            ]
        );
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
    }

    #[test]
    fn registrar_effects_tie_placeholder_camera_and_render_source_lifetimes() {
        let mut registrar = RenderSourceRegistrar::<FakePlaceholder>::default();
        let first = output(15, 1280);
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 40,
                    output: first.clone(),
                },
            ))
            .expect("expect add");
        let ready = registrar
            .apply(KmsRenderWorkerEvent::SourceReady {
                generation: 40,
                output: first.clone(),
                source: FakePlatform::source(&first, None),
            })
            .expect("ready first source");
        let handle = match ready.effects.as_slice() {
            [
                RegistrarEffect::Install {
                    generation: 40,
                    key,
                    handle,
                    placeholder,
                    ..
                },
            ] if *key == first.key && placeholder.extent() == (1280, 720) => *handle,
            _ => panic!("ready source must install one matching placeholder and camera handle"),
        };

        let mut changed = output(27, 1920);
        changed.key = first.key.clone();
        let changing = registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::ChangeOutput {
                    generation: 41,
                    output: changed.clone(),
                },
            ))
            .expect("expect change");
        assert!(matches!(
            changing.effects.as_slice(),
            [RegistrarEffect::Deactivate {
                generation: 41,
                key,
                handle: deactivated,
                ..
            }] if *key == first.key && *deactivated == Some(handle)
        ));
        let changed_ready = registrar
            .apply(KmsRenderWorkerEvent::SourceReady {
                generation: 41,
                output: changed.clone(),
                source: FakePlatform::source(&changed, None),
            })
            .expect("ready changed source");
        assert!(matches!(
            changed_ready.effects.as_slice(),
            [RegistrarEffect::Install {
                generation: 41,
                key,
                handle: installed,
                placeholder,
                ..
            }] if *key == first.key && *installed == handle && placeholder.extent() == (1920, 720)
        ));

        let removing = registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput {
                    generation: 42,
                    key: first.key.clone(),
                },
            ))
            .expect("expect removal");
        assert!(matches!(
            removing.effects.as_slice(),
            [RegistrarEffect::Deactivate {
                generation: 42,
                handle: deactivated,
                ..
            }] if *deactivated == Some(handle)
        ));
        let removed = registrar
            .apply(KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputRemoved {
                generation: 42,
                key: first.key.clone(),
            }))
            .expect("removed acknowledgement");
        assert!(matches!(
            removed.effects.as_slice(),
            [RegistrarEffect::Remove {
                generation: 42,
                handle: removed,
                ..
            }] if *removed == handle
        ));
        assert!(registrar.registered(&first.key).is_none());
    }

    #[test]
    fn failed_change_removes_the_inactive_source_lifecycle() {
        let mut registrar = RenderSourceRegistrar::<FakePlaceholder>::default();
        let first = output(16, 1280);
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 50,
                    output: first.clone(),
                },
            ))
            .expect("expect add");
        let ready = registrar
            .apply(KmsRenderWorkerEvent::SourceReady {
                generation: 50,
                output: first.clone(),
                source: FakePlatform::source(&first, None),
            })
            .expect("ready first source");
        let handle = match ready.effects.as_slice() {
            [RegistrarEffect::Install { handle, .. }] => *handle,
            _ => panic!("ready source must install one lifecycle"),
        };

        let mut changed = output(28, 1920);
        changed.key = first.key.clone();
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::ChangeOutput {
                    generation: 51,
                    output: changed,
                },
            ))
            .expect("expect change");
        let failed = registrar
            .apply(KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputFailed {
                generation: 51,
                key: first.key.clone(),
                reason: "injected change failure".into(),
            }))
            .expect("failed change");

        assert!(matches!(
            failed.effects.as_slice(),
            [RegistrarEffect::Remove {
                generation: 51,
                key,
                handle: removed,
                ..
            }] if *key == first.key && *removed == handle
        ));
        assert!(registrar.registered(&first.key).is_none());
    }

    #[test]
    fn placeholder_extent_must_equal_selected_vulkan_mode() {
        let mut registrar = RenderSourceRegistrar::<FakePlaceholder>::default();
        let output = output(5, 1280);
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 4,
                    output: output.clone(),
                },
            ))
            .expect("expect add");
        let error = match registrar.apply(KmsRenderWorkerEvent::SourceReady {
            generation: 4,
            output: output.clone(),
            source: RenderSource {
                placeholder: FakePlaceholder((640, 480)),
                acquire: Box::new(|| Err("unused".into())),
            },
        }) {
            Ok(_) => panic!("wrong-sized placeholder must be refused"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            RenderSourceRegistrarError::PlaceholderSizeMismatch {
                generation: 4,
                key: output.key,
                expected: (1280, 720),
                actual: (640, 480),
            }
        );
    }

    #[test]
    fn exhausted_manual_view_handle_returns_a_typed_output_failure() {
        let mut registrar = RenderSourceRegistrar::<FakePlaceholder> {
            next_handle: u32::MAX,
            ..Default::default()
        };
        let output = output(8, 1280);
        registrar
            .apply(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput {
                    generation: 5,
                    output: output.clone(),
                },
            ))
            .expect("expect add");
        let error = match registrar.apply(KmsRenderWorkerEvent::SourceReady {
            generation: 5,
            output: output.clone(),
            source: FakePlatform::source(&output, None),
        }) {
            Ok(_) => panic!("exhausted handle space must be refused"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            RenderSourceRegistrarError::HandleExhausted {
                generation: 5,
                key: output.key.clone(),
            }
        );
        assert_eq!(
            error.failure_reply(),
            KmsRenderReply::OutputFailed {
                generation: 5,
                key: output.key,
                reason: "manual-texture-view-handle-exhausted".into(),
            }
        );
    }

    #[test]
    fn disconnected_reply_channel_fail_stops_after_blocked_transition() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let barrier = Barrier::default();
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations,
                add_barrier: Some(barrier.clone()),
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let output = output(7, 1280);
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 8,
                output: output.clone(),
            })
            .expect("queue blocked add");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("accepted command"),
            KmsRenderWorkerEvent::CommandAccepted(_)
        ));
        barrier.wait_until_entered();
        drop(receiver);
        barrier.release();
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::ReplyChannelDisconnected {
                operation: KmsRenderOperation::AddOutput,
                generation: 8,
                key: Some(output.key),
            })
        );
    }

    #[test]
    fn disconnected_reply_channel_defers_source_cleanup_to_terminal_funnel() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let barrier = Barrier::default();
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations,
                add_barrier: Some(barrier.clone()),
                acquire_barrier: None,
                fail_remove: true,
            },
            events,
        )
        .expect("worker starts");
        let output = output(17, 1280);
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 18,
                output: output.clone(),
            })
            .expect("queue blocked add");
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("accepted command"),
            KmsRenderWorkerEvent::CommandAccepted(_)
        ));
        barrier.wait_until_entered();
        drop(receiver);
        barrier.release();
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::ReplyChannelDisconnected {
                operation: KmsRenderOperation::AddOutput,
                generation: 18,
                key: Some(output.key),
            })
        );
    }

    #[test]
    fn rejected_registration_uses_terminal_teardown_without_platform_rollback() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let registrations = worker.registration_sender();
        let output = output(71, 1280);
        let expected_key = output.key.clone();
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 72,
                output: output.clone(),
            })
            .expect("queue output");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::CommandAccepted(_)
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::SourceReady { generation: 72, .. }
        ));
        registrations
            .send(KmsRenderRegistration {
                generation: 72,
                key: output.key,
                disposition: KmsRenderRegistrationDisposition::Rejected,
            })
            .expect("reject source registration");

        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                failure: KmsRenderPlatformFailure {
                    code: "render-world-command-aborted",
                    ..
                },
                ..
            }))
        ));

        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderWorldHandoffAborted {
                operation: KmsRenderOperation::AddOutput,
                generation: 72,
                key: Some(expected_key),
            })
        );
        assert_eq!(
            *operations.lock().expect("operation log"),
            vec![(KmsRenderOperation::AddOutput, Some(71))]
        );
    }

    #[test]
    fn removal_failure_is_a_typed_output_failure_reply() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations,
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: true,
            },
            events,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let quiescences = worker.quiescence_sender();
        let key = output(3, 1280).key;
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 12,
                key: key.clone(),
            })
            .expect("queue failed removal");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 12,
                key: Some(key.clone()),
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("release failed removal source");
        quiescences
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 12,
                key: Some(key.clone()),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("failed removal source quiesced");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::CommandAccepted(_)
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputFailed {
                generation: 12,
                key: reply_key,
                reason,
            }) if reply_key == key && reason.starts_with("fake-remove-refused:")
        ));
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
    }

    struct UnprovenChangeReleasePlatform;

    impl KmsRenderPlatform for UnprovenChangeReleasePlatform {
        type Placeholder = FakePlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn change_output(
            &mut self,
            _output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Err(KmsRenderPlatformFailure::terminal(
                "kms-live-display-release-unproven",
                "injected vkReleaseDisplayEXT failure",
            ))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    #[test]
    fn change_output_with_unproven_release_stops_instead_of_replying_output_failed() {
        let (events, receiver) = mpsc::channel();
        let worker =
            KmsRenderWorker::spawn(UnprovenChangeReleasePlatform, events).expect("worker starts");
        let releases = worker.release_sender();
        let quiescences = worker.quiescence_sender();
        let output = output(13, 1920);
        worker
            .send(KmsRenderCommand::ChangeOutput {
                generation: 13,
                output: output.clone(),
            })
            .expect("queue failed output change");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::ChangeOutput,
                generation: 13,
                key: Some(output.key.clone()),
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("release changed output source");
        quiescences
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::ChangeOutput,
                generation: 13,
                key: Some(output.key.clone()),
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("changed output source quiesced");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::CommandAccepted(KmsRenderCommand::ChangeOutput {
                generation: 13,
                ..
            })
        ));
        let failure = match receiver.recv_timeout(Duration::from_secs(30)).unwrap() {
            KmsRenderWorkerEvent::WorkerFailed(failure) => failure,
            KmsRenderWorkerEvent::Reply(KmsRenderReply::OutputFailed { .. }) => {
                panic!("unproven display release must not be recoverable")
            }
            _ => panic!("unproven display release must terminate the worker"),
        };
        assert_eq!(failure.operation, KmsRenderOperation::ChangeOutput);
        assert_eq!(failure.failure.code, "kms-live-display-release-unproven");
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::PlatformFailed(failure))
        );
    }

    struct SuspendFailurePlatform;

    struct SuspendOrderingPlatform {
        worlds_clear: Arc<AtomicBool>,
        retirement_entered: mpsc::SyncSender<()>,
        retirement_release: Receiver<()>,
        submitted_work_retired: Arc<AtomicBool>,
        platform_clear: Arc<AtomicBool>,
    }

    impl KmsRenderPlatform for SuspendOrderingPlatform {
        type Placeholder = FakePlaceholder;

        fn retire_submitted_work(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            assert!(
                self.worlds_clear.load(Ordering::Acquire),
                "submitted work cannot retire before both-world quiescence"
            );
            self.retirement_entered
                .send(())
                .expect("observe submitted-work retirement");
            self.retirement_release
                .recv()
                .expect("complete submitted-work retirement");
            self.submitted_work_retired.store(true, Ordering::Release);
            Ok(())
        }

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            assert!(
                self.worlds_clear.load(Ordering::Acquire),
                "platform suspend cannot precede both-world quiescence"
            );
            assert!(
                self.submitted_work_retired.load(Ordering::Acquire),
                "platform suspend cannot destroy a surface before submitted work retires"
            );
            self.platform_clear.store(true, Ordering::Release);
            Ok(())
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    #[test]
    fn suspended_reply_follows_world_quiescence_gpu_retirement_and_platform_clear() {
        let worlds_clear = Arc::new(AtomicBool::new(false));
        let submitted_work_retired = Arc::new(AtomicBool::new(false));
        let platform_clear = Arc::new(AtomicBool::new(false));
        let (retirement_entered, observe_retirement) = mpsc::sync_channel(1);
        let (release_retirement, retirement_release) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            SuspendOrderingPlatform {
                worlds_clear: Arc::clone(&worlds_clear),
                retirement_entered,
                retirement_release,
                submitted_work_retired: Arc::clone(&submitted_work_retired),
                platform_clear: Arc::clone(&platform_clear),
            },
            events,
        )
        .expect("worker starts");
        worker
            .send(KmsRenderCommand::Suspend { generation: 70 })
            .expect("queue suspend");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            KmsRenderWorkerEvent::CommandAccepted(KmsRenderCommand::Suspend { generation: 70 })
        ));
        worker
            .release_sender()
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::Suspend,
                generation: 70,
                key: None,
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("main world cleared");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(5)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        worlds_clear.store(true, Ordering::Release);
        worker
            .quiescence_sender()
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::Suspend,
                generation: 70,
                key: None,
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("render world cleared");
        observe_retirement
            .recv_timeout(Duration::from_secs(1))
            .expect("submitted-work retirement starts after quiescence");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(5)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(!submitted_work_retired.load(Ordering::Acquire));
        assert!(!platform_clear.load(Ordering::Acquire));
        release_retirement
            .send(())
            .expect("submitted GPU work retires");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            KmsRenderWorkerEvent::Reply(KmsRenderReply::Suspended { generation: 70 })
        ));
        assert!(submitted_work_retired.load(Ordering::Acquire));
        assert!(platform_clear.load(Ordering::Acquire));
        assert_eq!(
            worker.finish(Duration::from_secs(1)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
    }

    impl KmsRenderPlatform for SuspendFailurePlatform {
        type Placeholder = FakePlaceholder;

        fn suspend(&mut self) -> Result<(), KmsRenderPlatformFailure> {
            Err(KmsRenderPlatformFailure::new(
                "fake-suspend-refused",
                "injected suspend failure",
            ))
        }

        fn resume(&mut self, _generation: u64) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }

        fn add_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn change_output(
            &mut self,
            output: &SelectedOutput,
        ) -> Result<RenderSource<Self::Placeholder>, KmsRenderPlatformFailure> {
            Ok(FakePlatform::source(output, None))
        }

        fn remove_output(&mut self, _key: &OutputKey) -> Result<(), KmsRenderPlatformFailure> {
            Ok(())
        }
    }

    #[test]
    fn suspend_failure_is_reported_and_stops_the_worker() {
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(SuspendFailurePlatform, events).expect("worker starts");
        let releases = worker.release_sender();
        let quiescences = worker.quiescence_sender();
        worker
            .send(KmsRenderCommand::Suspend { generation: 70 })
            .expect("queue suspend");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::Suspend,
                generation: 70,
                key: None,
                outcome: KmsRenderReleaseOutcome::Granted,
            })
            .expect("release suspended sources");
        quiescences
            .send(KmsRenderQuiescence {
                operation: KmsRenderOperation::Suspend,
                generation: 70,
                key: None,
                outcome: KmsRenderQuiescenceOutcome::Quiesced,
            })
            .expect("suspended sources quiesced");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            KmsRenderWorkerEvent::CommandAccepted(KmsRenderCommand::Suspend { generation: 70 })
        ));
        let failure = match receiver.recv_timeout(Duration::from_secs(30)).unwrap() {
            KmsRenderWorkerEvent::WorkerFailed(failure) => failure,
            _ => panic!("suspend failure must be typed"),
        };
        assert_eq!(failure.operation, KmsRenderOperation::Suspend);
        assert_eq!(failure.failure.code, "fake-suspend-refused");
        let reported = RenderSourceRegistrar::<FakePlaceholder>::default()
            .apply(KmsRenderWorkerEvent::WorkerFailed(failure.clone()))
            .expect("terminal failure is reportable");
        assert!(matches!(
            reported.reply,
            Some(KmsRenderReply::WorkerFailed {
                operation: KmsRenderOperation::Suspend,
                generation: 70,
                code: "fake-suspend-refused",
                ..
            })
        ));
        assert_eq!(
            worker.finish(Duration::from_secs(30)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::PlatformFailed(failure))
        );
    }

    #[test]
    fn aborted_release_stops_worker_and_fails_buffered_successor() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let key = output(80, 1280).key;
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 80,
                key: key.clone(),
            })
            .expect("queue aborted removal");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput { generation: 80, .. }
            ))
        ));
        worker
            .send(KmsRenderCommand::Resume { generation: 81 })
            .expect("queue successor");
        releases
            .send(KmsRenderRelease {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 80,
                key: Some(key.clone()),
                outcome: KmsRenderReleaseOutcome::Aborted,
            })
            .expect("abort removal");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 80,
                key: Some(failed_key),
                failure: KmsRenderPlatformFailure {
                    code: "render-world-command-aborted",
                    ..
                },
            })) if failed_key == key
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::Resume,
                generation: 81,
                key: None,
                failure: KmsRenderPlatformFailure {
                    code: "render-worker-stopped-before-command",
                    ..
                },
            }))
        ));
        assert!(operations.lock().expect("operation log").is_empty());
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::RenderWorldHandoffAborted {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 80,
                key: Some(key),
            })
        );
    }

    #[test]
    fn unexpected_registrar_release_publishes_the_in_flight_terminal_failure() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let releases = worker.release_sender();
        let key = output(86, 1280).key;
        let expected = KmsRenderRelease {
            operation: KmsRenderOperation::RemoveOutput,
            generation: 86,
            key: Some(key.clone()),
            outcome: KmsRenderReleaseOutcome::Granted,
        };
        let actual = KmsRenderRelease {
            generation: 87,
            ..expected.clone()
        };
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 86,
                key: key.clone(),
            })
            .expect("queue removal");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput { generation: 86, .. }
            ))
        ));
        releases
            .send(actual.clone())
            .expect("send mismatched registrar release");

        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                operation: KmsRenderOperation::RemoveOutput,
                generation: 86,
                key: Some(failed_key),
                failure: KmsRenderPlatformFailure {
                    code: "unexpected-registrar-release",
                    detail,
                    ..
                },
            })) if failed_key == key && detail.contains("generation: 87")
        ));
        assert!(operations.lock().expect("operation log").is_empty());
        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::UnexpectedRegistrarRelease {
                expected,
                actual,
            })
        );
    }

    #[test]
    fn finish_rejects_a_buffered_command_after_cancellation_becomes_observable() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let resume_barrier = Barrier::default();
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            BlockingResumePlatform {
                operations: Arc::clone(&operations),
                resume_barrier: resume_barrier.clone(),
            },
            events,
        )
        .expect("worker starts");
        worker
            .send(KmsRenderCommand::Resume { generation: 82 })
            .expect("queue blocking resume");
        resume_barrier.wait_until_entered();
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 83,
                output: output(83, 1280),
            })
            .expect("buffer successor before finish");
        let cancelled = Arc::clone(&worker.cancelled);
        let finish = thread::spawn(move || worker.finish(Duration::from_secs(2)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cancelled.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "finish did not publish cancellation before deadline"
            );
            thread::yield_now();
        }
        resume_barrier.release();

        assert_eq!(
            finish.join().expect("finish thread exits"),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
        assert_eq!(
            *operations.lock().expect("operation log"),
            vec![(KmsRenderOperation::Resume, None)],
            "the buffered add must not reach the platform after cancellation"
        );
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    KmsRenderWorkerEvent::WorkerFailed(KmsRenderWorkerFailure {
                        operation: KmsRenderOperation::AddOutput,
                        generation: 83,
                        failure: KmsRenderPlatformFailure {
                            code: "render-worker-stopped-before-command",
                            ..
                        },
                        ..
                    })
                ))
                .count(),
            1,
            "the dequeued successor must receive exactly one terminal answer"
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            KmsRenderWorkerEvent::CommandAccepted(KmsRenderCommand::AddOutput {
                generation: 83,
                ..
            })
        )));
    }

    #[test]
    fn cancellation_during_registration_wait_defers_cleanup_to_terminal_funnel() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 84,
                output: output(84, 1280),
            })
            .expect("queue add");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::AddOutput { generation: 84, .. }
            ))
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::SourceReady { generation: 84, .. })
        ));

        assert_eq!(
            worker.finish(Duration::from_secs(2)),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
        assert_eq!(
            *operations.lock().expect("operation log"),
            vec![(KmsRenderOperation::AddOutput, Some(84))]
        );
    }

    #[test]
    fn blocked_handshake_wakes_only_for_the_in_band_cancel_event() {
        let (sender, receiver) = mpsc::channel();
        let (entered_sender, entered_receiver) = mpsc::channel();
        let waiter = thread::spawn(move || {
            entered_sender.send(()).expect("announce blocking receive");
            wait_for_handshake::<KmsRenderRelease>(&receiver)
        });
        entered_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("handshake waiter starts");

        sender
            .send(WorkerInbound::Cancel)
            .expect("send in-band cancellation without a release payload");
        assert!(matches!(
            waiter.join().expect("handshake waiter exits"),
            HandshakeWait::Cancelled
        ));
    }

    #[test]
    fn finish_cancels_a_release_wait_even_while_sender_clones_are_held() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let (events, receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations: Arc::clone(&operations),
                add_barrier: None,
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        let _held_release_sender = worker.release_sender();
        let _held_registration_sender = worker.registration_sender();
        worker
            .send(KmsRenderCommand::RemoveOutput {
                generation: 81,
                key: output(81, 1280).key,
            })
            .expect("queue blocked removal");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(KmsRenderWorkerEvent::CommandAccepted(
                KmsRenderCommand::RemoveOutput { generation: 81, .. }
            ))
        ));

        let (finish_sender, finish_receiver) = mpsc::sync_channel(1);
        let finish_thread = thread::spawn(move || {
            let _ = finish_sender.send(worker.finish(Duration::from_millis(20)));
        });
        assert_eq!(
            finish_receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("release-wait cancellation returns before the test deadline"),
            KmsRenderJoinOutcome::Exited(KmsRenderWorkerExit::Cancelled)
        );
        finish_thread
            .join()
            .expect("release-wait finish caller exits");
        assert!(
            operations.lock().expect("operation log").is_empty(),
            "cancellation must wake the release wait without release traffic"
        );
    }

    #[test]
    fn finish_joins_an_overdue_platform_call_instead_of_detaching_it() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let add_barrier = Barrier::default();
        let (events, _receiver) = mpsc::channel();
        let worker = KmsRenderWorker::spawn(
            FakePlatform {
                operations,
                add_barrier: Some(add_barrier.clone()),
                acquire_barrier: None,
                fail_remove: false,
            },
            events,
        )
        .expect("worker starts");
        worker
            .send(KmsRenderCommand::AddOutput {
                generation: 82,
                output: output(82, 1280),
            })
            .expect("queue blocking platform call");
        add_barrier.wait_until_entered();
        let delayed_release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            add_barrier.release();
        });

        let started = Instant::now();
        let outcome = worker.finish(Duration::from_millis(5));
        let finish_elapsed = started.elapsed();
        delayed_release.join().expect("delayed release exits");

        assert_eq!(outcome, KmsRenderJoinOutcome::TimedOut);
        assert!(
            finish_elapsed >= Duration::from_millis(40),
            "finish returned before the overdue worker thread terminated"
        );
    }
}
