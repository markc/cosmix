//! `comp.*` Bus citizen worker and its bounded protocol ingress.

use std::{
    fmt::Write as _,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cosmix_bus::bus::BusMessage;
use cosmix_client::{ConnState, SupervisedClient, SupervisedError};
use serde_json::Value;
use smithay::reexports::calloop::channel;
use tokio::{
    sync::{Semaphore, mpsc as tokio_mpsc, watch},
    task::JoinSet,
};

use crate::{decoration::DecorationStartup, protocol::port_snapshot};
use port_snapshot::{
    BROKER_CONNECTED, BROKER_RETRYING, CompSnapshot, MAX_REPLY_BODY_BYTES, MAX_REPLY_WIRE_BYTES,
    SnapshotContext, dispatch_read, error, too_large,
};

pub(crate) const PORT_QUEUE_CAPACITY: usize = 16;
const PORT_REPLY_CAPACITY: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const PORT_SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
const CLIENT_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);
const DEREGISTER_BUDGET: Duration = Duration::from_millis(200);
const CLOSE_BUDGET: Duration = Duration::from_millis(50);

pub(crate) enum PortCommand {
    Snapshot(PortRequest),
}

pub(crate) struct PortRequest {
    pub(crate) reply: tokio::sync::oneshot::Sender<Arc<CompSnapshot>>,
}

#[derive(Clone)]
pub(crate) struct PortIngress {
    sender: channel::SyncSender<PortCommand>,
    queue_depth: Arc<AtomicUsize>,
}

impl PortIngress {
    pub(crate) fn request_snapshot(&self) -> Result<SnapshotAdmission, ()> {
        let mut depth = self.queue_depth.load(Ordering::Acquire);
        loop {
            if depth >= PORT_QUEUE_CAPACITY {
                return Err(());
            }
            match self.queue_depth.compare_exchange_weak(
                depth,
                depth + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => depth = observed,
            }
        }
        let (reply, receive) = tokio::sync::oneshot::channel();
        match self
            .sender
            .try_send(PortCommand::Snapshot(PortRequest { reply }))
        {
            Ok(()) => Ok(SnapshotAdmission {
                receive,
                depth: QueueDepthGuard(Arc::clone(&self.queue_depth)),
            }),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn depth_for_test(&self) -> usize {
        self.queue_depth.load(Ordering::Acquire)
    }
}

pub(crate) struct SnapshotAdmission {
    receive: tokio::sync::oneshot::Receiver<Arc<CompSnapshot>>,
    depth: QueueDepthGuard,
}

impl SnapshotAdmission {
    pub(crate) async fn receive(self) -> Result<Arc<CompSnapshot>, ()> {
        let Self { receive, depth } = self;
        let result = match tokio::time::timeout(SNAPSHOT_TIMEOUT, receive).await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(_)) | Err(_) => Err(()),
        };
        drop(depth);
        result
    }
}

pub(crate) struct PortProtocolWiring {
    pub(crate) source: channel::Channel<PortCommand>,
    pub(crate) context: Arc<SnapshotContext>,
}

#[cfg(test)]
pub(crate) fn test_wiring(context: Arc<SnapshotContext>) -> (PortProtocolWiring, PortIngress) {
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: context.queue_depth.clone(),
    };
    (PortProtocolWiring { source, context }, ingress)
}

pub(crate) struct PortStarter {
    service: String,
    noded_url: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
}

pub(crate) struct PortWorker {
    shutdown: watch::Sender<bool>,
    ingress: Option<PortIngress>,
    completion: Mutex<Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn prepare(
    service: String,
    backend: &'static str,
    decoration: &DecorationStartup,
) -> Result<(PortProtocolWiring, PortStarter), String> {
    validate_service_name(&service)?;
    let noded_url = cosmix_config::client_helpers::resolve_noded_url();
    let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
    let queue_depth = Arc::new(AtomicUsize::new(0));
    let reply_timeouts = Arc::new(AtomicU64::new(0));
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: queue_depth.clone(),
    };
    let build = cosmix_buildinfo::build_info!();
    let context = Arc::new(SnapshotContext {
        service: Arc::from(service.as_str()),
        version: Arc::from(build.version),
        backend,
        engine: "bevy-0.19/wgpu",
        instance: Arc::from(random_instance_id()?.as_str()),
        decoration_enabled: decoration.enabled,
        decoration_style: decoration.theme.style.name(),
        broker: broker.clone(),
        queue_depth,
        reply_timeouts: reply_timeouts.clone(),
    });
    Ok((
        PortProtocolWiring { source, context },
        PortStarter {
            service,
            noded_url,
            ingress,
            broker,
            reply_timeouts,
        },
    ))
}

impl PortStarter {
    pub(crate) fn start(self) -> Result<PortWorker, String> {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (completion_tx, completion) = mpsc::sync_channel(1);
        let thread_ingress = self.ingress.clone();
        let service = self.service;
        let noded_url = self.noded_url;
        let broker = self.broker;
        let reply_timeouts = self.reply_timeouts;
        let thread = thread::Builder::new()
            .name("cosmix-comp-port".into())
            .spawn(move || {
                let _completion = CompletionOnDrop(completion_tx);
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "failed to build compositor Bus runtime");
                        return;
                    }
                };
                let connect_service = service.clone();
                let connect_url = noded_url;
                runtime.block_on(worker_loop(
                    service,
                    thread_ingress,
                    broker,
                    reply_timeouts,
                    shutdown_rx,
                    move || {
                        let service = connect_service.clone();
                        let url = connect_url.clone();
                        async move {
                            SupervisedClient::connect_options(&service, &url)
                                .fatal_on_registration_rejection(true)
                                .connect()
                                .await
                                .map_err(|error| classify_connect_error(&service, error))
                        }
                    },
                ));
            })
            .map_err(|error| format!("failed to spawn compositor Bus worker: {error}"))?;
        Ok(PortWorker {
            shutdown,
            ingress: Some(self.ingress),
            completion: Mutex::new(completion),
            thread: Some(thread),
        })
    }
}

impl PortWorker {
    pub(crate) fn begin_shutdown(&mut self) {
        let _ = self.shutdown.send(true);
        self.ingress.take();
    }

    pub(crate) fn finish(mut self) {
        self.begin_shutdown();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let completion = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match completion.recv_timeout(PORT_SHUTDOWN_GRACE) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if thread.join().is_err() {
                    tracing::error!("compositor Bus worker panicked during shutdown");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    grace_ms = PORT_SHUTDOWN_GRACE.as_millis(),
                    "compositor Bus worker did not stop in time and was detached"
                );
                drop(thread);
            }
        }
    }
}

struct CompletionOnDrop(mpsc::SyncSender<()>);

impl Drop for CompletionOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

struct QueueDepthGuard(Arc<AtomicUsize>);

impl Drop for QueueDepthGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
enum ConnectAttemptError {
    Retry(String),
    RegistrationRejected {
        service: String,
        rc: u8,
        message: String,
    },
}

enum ConnectOutcome<C> {
    Connected(C),
    RegistrationRejected {
        service: String,
        rc: u8,
        message: String,
    },
    Shutdown,
}

async fn connect_loop<F, Fut, C>(
    shutdown: &mut watch::Receiver<bool>,
    broker: &AtomicU8,
    mut connector: F,
    minimum_delay: Duration,
) -> ConnectOutcome<C>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<C, ConnectAttemptError>>,
{
    let mut attempt = 0_u32;
    loop {
        broker.store(BROKER_RETRYING, Ordering::Release);
        let result = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return ConnectOutcome::Shutdown;
                }
                continue;
            }
            result = connector() => result,
        };
        match result {
            Ok(client) => return ConnectOutcome::Connected(client),
            Err(ConnectAttemptError::RegistrationRejected {
                service,
                rc,
                message,
            }) => {
                return ConnectOutcome::RegistrationRejected {
                    service,
                    rc,
                    message,
                };
            }
            Err(ConnectAttemptError::Retry(message)) => {
                tracing::debug!(attempt, error = %message, "compositor Bus connect failed; retrying");
                let exponential = 250_u64.saturating_mul(1_u64 << attempt.min(16)).min(30_000);
                let delay = minimum_delay.max(Duration::from_millis(exponential));
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return ConnectOutcome::Shutdown;
                        }
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

type WorkerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait WorkerClient: Send + Sync + 'static {
    fn incoming(&self) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>>;
    fn state(&self) -> ConnState;
    fn subscribe_state(&self) -> watch::Receiver<ConnState>;
    fn respond_parts<'a>(&'a self, reply: &'a PendingReply)
    -> WorkerFuture<'a, Result<(), String>>;
    fn deregister(&self) -> WorkerFuture<'_, Result<(), String>>;
    fn close(&self) -> WorkerFuture<'_, ()>;
}

impl WorkerClient for SupervisedClient {
    fn incoming(&self) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>> {
        SupervisedClient::incoming(self)
    }

    fn state(&self) -> ConnState {
        SupervisedClient::state(self)
    }

    fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        SupervisedClient::subscribe_state(self)
    }

    fn respond_parts<'a>(
        &'a self,
        reply: &'a PendingReply,
    ) -> WorkerFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.respond_parts(
                &reply.from,
                &reply.command,
                reply.id.as_deref(),
                reply.rc,
                &reply.body,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    fn deregister(&self) -> WorkerFuture<'_, Result<(), String>> {
        Box::pin(async move {
            SupervisedClient::deregister(self)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn close(&self) -> WorkerFuture<'_, ()> {
        Box::pin(SupervisedClient::close(self))
    }
}

struct PendingReply {
    from: String,
    command: String,
    id: Option<String>,
    rc: u8,
    body: Arc<str>,
}

impl PendingReply {
    fn new(command: cosmix_client::IncomingCommand, (rc, body): (u8, Arc<str>)) -> Self {
        Self {
            from: command.from,
            command: command.command,
            id: command.id,
            rc,
            body,
        }
    }
}

async fn worker_loop<F, Fut, C>(
    service: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
    connector: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<C, ConnectAttemptError>>,
    C: WorkerClient,
{
    let outcome = connect_loop(&mut shutdown, &broker, connector, Duration::ZERO).await;
    let client = match outcome {
        ConnectOutcome::Connected(client) => Arc::new(client),
        ConnectOutcome::RegistrationRejected {
            service,
            rc,
            message,
        } => {
            tracing::error!(service = %service, rc, %message, "Bus registration rejected; compositor continues without a port");
            return;
        }
        ConnectOutcome::Shutdown => return,
    };
    let Some(mut incoming) = client.incoming() else {
        tracing::error!(service = %service, "compositor Bus incoming stream was already taken");
        return;
    };
    let mut states = client.subscribe_state();
    apply_connection_state(&broker, *states.borrow());
    let mut responders = JoinSet::new();
    let responder_permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
    let (reply_sender, reply_receiver) = tokio_mpsc::channel(PORT_REPLY_CAPACITY);
    let reply_task = tokio::spawn(reply_loop(
        Arc::clone(&client),
        Arc::from(service.as_str()),
        reply_receiver,
        Arc::clone(&reply_timeouts),
    ));

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            changed = states.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = *states.borrow_and_update();
                apply_connection_state(&broker, state);
                if state == ConnState::Fatal {
                    tracing::error!(service = %service, "Bus registration rejected during reconnect; compositor continues without a port");
                    break;
                }
            }
            command = incoming.recv() => {
                let Some(command) = command else {
                    let state = client.state();
                    apply_connection_state(&broker, state);
                    if state == ConnState::Fatal {
                        tracing::error!(service = %service, "Bus registration rejected during reconnect; compositor continues without a port");
                    }
                    break;
                };
                handle_incoming(
                    &ingress,
                    &mut responders,
                    &responder_permits,
                    &reply_sender,
                    &reply_timeouts,
                    command,
                );
            }
            completed = responders.join_next(), if !responders.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::debug!(%error, "compositor Bus responder task stopped");
                }
            }
        }
    }

    responders.abort_all();
    while responders.join_next().await.is_some() {}
    drop(reply_sender);
    reply_task.abort();
    let _ = reply_task.await;
    graceful_client_shutdown(client.as_ref()).await;
}

fn classify_connect_error(service: &str, error: SupervisedError) -> ConnectAttemptError {
    if let Some((rc, message)) = error.registration_rejection() {
        ConnectAttemptError::RegistrationRejected {
            service: service.to_string(),
            rc,
            message: message.to_string(),
        }
    } else {
        ConnectAttemptError::Retry(error.to_string())
    }
}

async fn graceful_client_shutdown<C: WorkerClient>(client: &C) {
    debug_assert_eq!(CLIENT_SHUTDOWN_BUDGET, DEREGISTER_BUDGET + CLOSE_BUDGET);
    match tokio::time::timeout(DEREGISTER_BUDGET, client.deregister()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::debug!(%error, "compositor Bus deregister did not complete cleanly")
        }
        Err(_) => tracing::debug!(
            timeout_ms = DEREGISTER_BUDGET.as_millis(),
            "compositor Bus deregister timed out"
        ),
    }
    if tokio::time::timeout(CLOSE_BUDGET, client.close())
        .await
        .is_err()
    {
        tracing::debug!(
            timeout_ms = CLOSE_BUDGET.as_millis(),
            "compositor Bus close timed out"
        );
    }
}

fn apply_connection_state(broker: &AtomicU8, state: ConnState) {
    broker.store(
        if state == ConnState::Connected {
            BROKER_CONNECTED
        } else {
            BROKER_RETRYING
        },
        Ordering::Release,
    );
}

fn handle_incoming(
    ingress: &PortIngress,
    responders: &mut JoinSet<()>,
    responder_permits: &Arc<Semaphore>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    command: cosmix_client::IncomingCommand,
) {
    while let Some(completed) = responders.try_join_next() {
        if let Err(error) = completed {
            tracing::debug!(%error, "compositor Bus responder task stopped");
        }
    }
    let malformed =
        !command.body.is_empty() && serde_json::from_str::<Value>(&command.body).is_err();
    if command.command == "comp.ping" {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, (0, Arc::from("{\"pong\":true}"))),
        );
        return;
    }
    let needs_snapshot = matches!(
        command.command.as_str(),
        "comp.info" | "comp.props.get" | "comp.props.list" | "comp.props.describe"
    );
    if !needs_snapshot {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, error("unknown_verb")),
        );
        return;
    }
    if malformed {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, error("unknown_path")),
        );
        return;
    }
    let permit = match Arc::clone(responder_permits).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            return;
        }
    };
    let admission = match ingress.request_snapshot() {
        Ok(admission) => admission,
        Err(()) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            drop(permit);
            return;
        }
    };
    let reply_sender = reply_sender.clone();
    let reply_timeouts = Arc::clone(reply_timeouts);
    responders.spawn(async move {
        let _permit = permit;
        let reply = match admission.receive().await {
            Ok(snapshot) => {
                dispatch_read(snapshot, command.command.clone(), command.args.clone()).await
            }
            Err(()) => error("busy"),
        };
        queue_reply(
            &reply_sender,
            &reply_timeouts,
            PendingReply::new(command, reply),
        );
    });
}

fn queue_reply(
    sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &AtomicU64,
    reply: PendingReply,
) {
    if let Err(error) = sender.try_send(reply)
        && matches!(error, tokio_mpsc::error::TrySendError::Full(_))
    {
        reply_timeouts.fetch_add(1, Ordering::AcqRel);
    }
}

async fn reply_loop<C: WorkerClient>(
    client: Arc<C>,
    service: Arc<str>,
    mut replies: tokio_mpsc::Receiver<PendingReply>,
    reply_timeouts: Arc<AtomicU64>,
) {
    while let Some(reply) = replies.recv().await {
        let Some(reply) = enforce_reply_wire_limit(&service, reply) else {
            tracing::debug!(
                service = %service,
                "compositor Bus reply headers exceed the broker WebSocket frame cap"
            );
            continue;
        };
        match tokio::time::timeout(REPLY_SEND_TIMEOUT, client.respond_parts(&reply)).await {
            Err(_) => {
                reply_timeouts.fetch_add(1, Ordering::AcqRel);
                tracing::debug!(
                    command = %reply.command,
                    timeout_ms = REPLY_SEND_TIMEOUT.as_millis(),
                    "compositor Bus reply timed out"
                );
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, command = %reply.command, "compositor Bus reply failed");
            }
            Ok(Ok(())) => {}
        }
    }
}

/// Exact byte count produced by `NodedClient::respond_parts` for this reply.
/// The body stays borrowed: only the small canonical header block is assembled.
fn reply_wire_bytes(service: &str, reply: &PendingReply) -> usize {
    let mut message = BusMessage::new()
        .with_header("command", &reply.command)
        .with_header("from", service)
        .with_header("to", &reply.from)
        .with_header("type", "response")
        .with_header("rc", &reply.rc.to_string());
    if let Some(id) = reply.id.as_deref() {
        message = message.with_header("id", id);
    }
    let header_and_framing = message.to_wire().len();
    header_and_framing
        .checked_add(reply.body.len())
        .and_then(|bytes| {
            bytes.checked_add(usize::from(
                !reply.body.is_empty() && !reply.body.ends_with('\n'),
            ))
        })
        .unwrap_or(usize::MAX)
}

fn enforce_reply_wire_limit(service: &str, mut reply: PendingReply) -> Option<PendingReply> {
    if reply_wire_bytes(service, &reply) > MAX_REPLY_WIRE_BYTES {
        (reply.rc, reply.body) = too_large(MAX_REPLY_BODY_BYTES);
    }
    (reply_wire_bytes(service, &reply) <= MAX_REPLY_WIRE_BYTES).then_some(reply)
}

fn random_instance_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("failed to seed compositor Bus instance id: {error}"))?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    Ok(id)
}

pub(crate) fn validate_service_name(name: &str) -> Result<(), String> {
    let valid = (2..=31).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid Bus service name '{name}': expected ^[a-z][a-z0-9-]{{1,30}}$"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        future,
        sync::atomic::{AtomicBool, AtomicUsize},
    };

    struct FakeClient {
        incoming: Mutex<Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>>>,
        states: watch::Sender<ConnState>,
        hang_replies: bool,
        responses_started: Arc<AtomicUsize>,
        deregister_hangs: Arc<AtomicBool>,
        deregistered: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl FakeClient {
        fn new(
            initial_state: ConnState,
            hang_replies: bool,
        ) -> (
            Self,
            tokio_mpsc::UnboundedSender<cosmix_client::IncomingCommand>,
            watch::Sender<ConnState>,
        ) {
            let (commands, incoming) = tokio_mpsc::unbounded_channel();
            let (states, _) = watch::channel(initial_state);
            (
                Self {
                    incoming: Mutex::new(Some(incoming)),
                    states: states.clone(),
                    hang_replies,
                    responses_started: Arc::new(AtomicUsize::new(0)),
                    deregister_hangs: Arc::new(AtomicBool::new(false)),
                    deregistered: Arc::new(AtomicUsize::new(0)),
                    closed: Arc::new(AtomicUsize::new(0)),
                },
                commands,
                states,
            )
        }
    }

    impl WorkerClient for FakeClient {
        fn incoming(
            &self,
        ) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>> {
            self.incoming
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }

        fn state(&self) -> ConnState {
            *self.states.borrow()
        }

        fn subscribe_state(&self) -> watch::Receiver<ConnState> {
            self.states.subscribe()
        }

        fn respond_parts<'a>(
            &'a self,
            _reply: &'a PendingReply,
        ) -> WorkerFuture<'a, Result<(), String>> {
            self.responses_started.fetch_add(1, Ordering::AcqRel);
            if self.hang_replies {
                Box::pin(future::pending())
            } else {
                Box::pin(future::ready(Ok(())))
            }
        }

        fn deregister(&self) -> WorkerFuture<'_, Result<(), String>> {
            self.deregistered.fetch_add(1, Ordering::AcqRel);
            if self.deregister_hangs.load(Ordering::Acquire) {
                Box::pin(future::pending())
            } else {
                Box::pin(future::ready(Ok(())))
            }
        }

        fn close(&self) -> WorkerFuture<'_, ()> {
            self.closed.fetch_add(1, Ordering::AcqRel);
            Box::pin(future::ready(()))
        }
    }

    fn test_ingress() -> (PortIngress, channel::Channel<PortCommand>, Arc<AtomicUsize>) {
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
        (
            PortIngress {
                sender,
                queue_depth: Arc::clone(&queue_depth),
            },
            source,
            queue_depth,
        )
    }

    fn command(command: &str, id: usize) -> cosmix_client::IncomingCommand {
        cosmix_client::IncomingCommand {
            from: "test-caller".into(),
            command: command.into(),
            id: Some(id.to_string()),
            args: Value::Null,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    async fn wait_for_broker(broker: &AtomicU8, expected: u8) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while broker.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker publishes broker edge");
    }

    async fn wait_for_counter(counter: &AtomicUsize, expected: usize, message: &'static str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(message);
    }

    async fn next_port_command(source: &channel::Channel<PortCommand>) -> PortCommand {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match source.try_recv() {
                    Ok(command) => return command,
                    Err(mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("port source disconnected")
                    }
                }
            }
        })
        .await
        .expect("worker admits command")
    }

    #[tokio::test(start_paused = true)]
    async fn worker_refusal_stays_retrying_without_panicking() {
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        let broker = AtomicU8::new(BROKER_CONNECTED);
        let attempts = AtomicUsize::new(0);
        let (_sender, protocol_source) = channel::sync_channel::<PortCommand>(1);
        let result = connect_loop(
            &mut shutdown,
            &broker,
            || {
                attempts.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Err::<(), _>(ConnectAttemptError::Retry(
                    "connection refused".into(),
                )))
            },
            Duration::from_millis(1),
        );
        tokio::pin!(result);
        tokio::select! {
            _ = &mut result => panic!("refused connector must keep retrying"),
            _ = async {
                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_millis(250)).await;
                tokio::task::yield_now().await;
            } => {}
        }
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        assert!(attempts.load(Ordering::Relaxed) >= 1);
        assert!(matches!(
            protocol_source.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn complete_worker_loop_tracks_state_edges_without_polling() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));

        wait_for_broker(&broker, BROKER_CONNECTED).await;
        states.send_replace(ConnState::Disconnected);
        wait_for_broker(&broker, BROKER_RETRYING).await;
        states.send_replace(ConnState::Connected);
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn complete_worker_loop_terminates_on_fatal_reconnect_collision() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));

        wait_for_broker(&broker, BROKER_CONNECTED).await;
        states.send_replace(ConnState::Fatal);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("fatal reconnect collision terminates worker")
            .expect("worker exits cleanly");
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
    }

    #[tokio::test]
    async fn complete_worker_loop_terminates_on_registration_rejection_without_renaming() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_CONNECTED));
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_connector = Arc::clone(&attempts);
        let (_shutdown_tx, shutdown) = watch::channel(false);
        worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            shutdown,
            move || {
                attempts_for_connector.fetch_add(1, Ordering::Relaxed);
                future::ready(Err::<FakeClient, _>(
                    ConnectAttemptError::RegistrationRejected {
                        service: "comp-nested".into(),
                        rc: 10,
                        message: "registration refused".into(),
                    },
                ))
            },
        )
        .await;
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
    }

    #[test]
    fn service_name_validation_matches_abp_grammar() {
        for valid in ["comp", "comp-nested", "a0"] {
            assert!(validate_service_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["c", "Comp", "comp_nested", "-comp", "comp-é"] {
            assert!(validate_service_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn reply_headroom_covers_maximal_headers_and_exact_body_limit_fits() {
        let maximal_service = format!("a{}", "z".repeat(30));
        assert!(validate_service_name(&maximal_service).is_ok());
        let reply = PendingReply {
            from: maximal_service.clone(),
            command: "comp.props.describe".into(),
            id: Some(format!("noded-{}", u64::MAX)),
            rc: 0,
            body: Arc::from("x".repeat(MAX_REPLY_BODY_BYTES)),
        };
        let wire_bytes = reply_wire_bytes(&maximal_service, &reply);
        let header_and_framing = wire_bytes - reply.body.len();
        assert!(
            header_and_framing <= port_snapshot::REPLY_WIRE_HEADROOM_BYTES,
            "{header_and_framing} header/framing bytes exceed the documented reserve"
        );
        assert!(wire_bytes <= MAX_REPLY_WIRE_BYTES);

        let checked = enforce_reply_wire_limit(&maximal_service, reply)
            .expect("maximal canonical reply headers fit");
        assert_eq!(checked.rc, 0);
        assert_eq!(checked.body.len(), MAX_REPLY_BODY_BYTES);
    }

    #[test]
    fn measured_wire_overflow_becomes_too_large() {
        let reply = PendingReply {
            from: "requester".into(),
            command: "comp.props.get".into(),
            id: Some("noded-1".into()),
            rc: 0,
            body: Arc::from("x".repeat(MAX_REPLY_WIRE_BYTES)),
        };
        assert!(reply_wire_bytes("comp-nested", &reply) > MAX_REPLY_WIRE_BYTES);

        let checked =
            enforce_reply_wire_limit("comp-nested", reply).expect("too_large response fits");
        assert_eq!(checked.rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&checked.body).expect("too_large JSON"),
            serde_json::json!({
                "error": "too_large",
                "limit_bytes": MAX_REPLY_BODY_BYTES,
                "hint": "read a subtree",
            })
        );
        assert!(reply_wire_bytes("comp-nested", &checked) <= MAX_REPLY_WIRE_BYTES);
    }

    #[tokio::test]
    async fn ping_ignores_malformed_body_while_property_reads_reject_it() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let responder_permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));

        let mut ping = command("comp.ping", 1);
        ping.body = "{".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &responder_permits,
            &reply_sender,
            &reply_timeouts,
            ping,
        );
        let reply = replies.recv().await.expect("ping reply queued");
        assert_eq!(reply.rc, 0);
        assert_eq!(reply.body.as_ref(), "{\"pong\":true}");

        let mut get = command("comp.props.get", 2);
        get.body = "{".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &responder_permits,
            &reply_sender,
            &reply_timeouts,
            get,
        );
        let reply = replies.recv().await.expect("malformed read reply queued");
        assert_eq!(reply.rc, 10);
        assert_eq!(reply.body.as_ref(), "{\"error\":\"unknown_path\"}");
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn bounded_ingress_releases_depth_through_production_admission_completion() {
        let (ingress, source, queue_depth) = test_ingress();
        let mut admissions = Vec::new();
        for _ in 0..PORT_QUEUE_CAPACITY {
            admissions.push(ingress.request_snapshot().expect("request admitted"));
        }
        assert!(ingress.request_snapshot().is_err());
        assert_eq!(queue_depth.load(Ordering::Acquire), PORT_QUEUE_CAPACITY);

        for _ in 0..PORT_QUEUE_CAPACITY {
            let PortCommand::Snapshot(request) = source.try_recv().expect("staged request");
            drop(request);
        }
        for admission in admissions {
            assert!(admission.receive().await.is_err());
        }
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn completed_responders_are_reaped_and_seventeenth_command_is_admitted() {
        let (ingress, source, queue_depth) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        for id in 0..PORT_QUEUE_CAPACITY {
            commands
                .send(command("comp.info", id))
                .expect("worker live");
        }
        for _ in 0..PORT_QUEUE_CAPACITY {
            let PortCommand::Snapshot(request) = next_port_command(&source).await;
            drop(request);
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue_depth.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("production responder completion releases all admissions");

        for id in 100..164 {
            commands
                .send(command("comp.ping", id))
                .expect("worker live");
        }
        commands
            .send(command("comp.info", PORT_QUEUE_CAPACITY))
            .expect("worker live");
        let PortCommand::Snapshot(request) = next_port_command(&source).await;
        drop(request);

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn worker_loop_abandons_black_holed_replies_and_stays_responsive() {
        let (ingress, source, queue_depth) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, true);
        let responses_started = Arc::clone(&client.responses_started);
        let mut client = Some(client);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::clone(&reply_timeouts),
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        commands
            .send(command("comp.ping", 1))
            .expect("worker admits ping");
        commands
            .send(command("comp.unknown", 2))
            .expect("worker admits error reply");
        wait_for_counter(&responses_started, 1, "first reply send starts").await;
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);

        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        wait_for_counter(&responses_started, 2, "second reply send starts").await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 1);
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 2);
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);

        commands
            .send(command("comp.info", 3))
            .expect("worker remains responsive");
        let PortCommand::Snapshot(request) = next_port_command(&source).await;
        assert_eq!(queue_depth.load(Ordering::Acquire), 1);
        drop(request);
        wait_for_counter(&queue_depth, 0, "later admission releases depth").await;
        wait_for_counter(&responses_started, 3, "later error reply send starts").await;
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 3);

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn reply_sender_abandons_black_holed_reply_after_deadline_and_counts_it() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, true);
        let client = Arc::new(client);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = tokio_mpsc::channel(1);
        let task = tokio::spawn(reply_loop(
            client,
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&reply_timeouts),
        ));
        sender
            .send(PendingReply::new(
                command("comp.ping", 1),
                (0, Arc::from("{}")),
            ))
            .await
            .expect("reply lane open");
        tokio::task::yield_now().await;
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 1);
        drop(sender);
        task.await.expect("reply sender exits");
    }

    #[tokio::test]
    async fn graceful_shutdown_deregisters_then_closes_when_broker_answers() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let deregistered = Arc::clone(&client.deregistered);
        let closed = Arc::clone(&client.closed);

        graceful_client_shutdown(&client).await;

        assert_eq!(deregistered.load(Ordering::Acquire), 1);
        assert_eq!(closed.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_shutdown_closes_within_budget_when_deregister_hangs() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.deregister_hangs.store(true, Ordering::Release);
        let deregistered = Arc::clone(&client.deregistered);
        let closed = Arc::clone(&client.closed);
        let shutdown = tokio::spawn(async move {
            graceful_client_shutdown(&client).await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CLIENT_SHUTDOWN_BUDGET).await;

        shutdown.await.expect("bounded shutdown completes");
        assert_eq!(deregistered.load(Ordering::Acquire), 1);
        assert_eq!(closed.load(Ordering::Acquire), 1);
    }
}
