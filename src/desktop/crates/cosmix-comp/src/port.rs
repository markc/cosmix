//! `comp.*` Bus citizen worker and its bounded protocol ingress.

use std::{
    collections::BTreeMap,
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
use serde_json::{Value, json};
use smithay::reexports::calloop::channel;
use tokio::{
    sync::{Semaphore, mpsc as tokio_mpsc, watch},
    task::JoinSet,
};

use crate::{
    decoration::DecorationStartup,
    protocol::{port_observation, port_snapshot},
};
use port_observation::{ObservationProducer, ObservationRecord};
use port_snapshot::{
    BROKER_CONNECTED, BROKER_RETRYING, CompSnapshot, MAX_REPLY_BODY_BYTES, MAX_REPLY_WIRE_BYTES,
    SnapshotContext, dispatch_read, error, too_large,
};

pub(crate) const PORT_QUEUE_CAPACITY: usize = 16;
const PORT_REPLY_CAPACITY: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);
const PUBLISH_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PORT_SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
const CLIENT_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);
const DEREGISTER_BUDGET: Duration = Duration::from_millis(200);
const CLOSE_BUDGET: Duration = Duration::from_millis(50);

pub(crate) enum PortCommand {
    Snapshot(PortRequest),
    Watch(PortReply),
    Set(PortSetRequest),
    WatchState { active: bool, order: u64 },
}

pub(crate) struct PortRequest {
    pub(crate) reply: tokio::sync::oneshot::Sender<Arc<CompSnapshot>>,
}

pub(crate) struct PortReply {
    pub(crate) order: u64,
    pub(crate) reply: tokio::sync::oneshot::Sender<(u8, Arc<str>)>,
}

pub(crate) struct PortSetRequest {
    pub(crate) order: u64,
    pub(crate) path: String,
    pub(crate) value: Value,
    pub(crate) reply: tokio::sync::oneshot::Sender<(u8, Arc<str>)>,
}

pub(crate) enum PortControl {
    Watch(PortReply),
    Set(PortSetRequest),
    WatchState { active: bool, order: u64 },
}

impl PortControl {
    pub(crate) fn order(&self) -> u64 {
        match self {
            Self::Watch(request) => request.order,
            Self::Set(request) => request.order,
            Self::WatchState { order, .. } => *order,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PortIngress {
    sender: channel::SyncSender<PortCommand>,
    queue_depth: Arc<AtomicUsize>,
    control_order: Arc<AtomicU64>,
    pending_idle_order: Arc<AtomicU64>,
}

impl PortIngress {
    pub(crate) fn request_snapshot(&self) -> Result<SnapshotAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(PortCommand::Snapshot(PortRequest { reply }), receive)
            .map(SnapshotAdmission)
    }

    pub(crate) fn request_watch(&self) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        let order = self.next_control_order();
        self.admit(PortCommand::Watch(PortReply { order, reply }), receive)
    }

    pub(crate) fn request_set(&self, path: String, value: Value) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(
            PortCommand::Set(PortSetRequest {
                order: self.next_control_order(),
                path,
                value,
                reply,
            }),
            receive,
        )
    }

    pub(crate) fn set_watch_state(&self, active: bool) {
        let order = self.next_control_order();
        if let Err(TrySendError::Full(_)) = self
            .sender
            .try_send(PortCommand::WatchState { active, order })
            && !active
        {
            self.pending_idle_order.fetch_max(order, Ordering::AcqRel);
        }
    }

    fn next_control_order(&self) -> u64 {
        self.control_order
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    fn admit<T>(
        &self,
        command: PortCommand,
        receive: tokio::sync::oneshot::Receiver<T>,
    ) -> Result<Admission<T>, ()> {
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
        match self.sender.try_send(command) {
            Ok(()) => Ok(Admission {
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

pub(crate) struct Admission<T> {
    receive: tokio::sync::oneshot::Receiver<T>,
    depth: QueueDepthGuard,
}

impl<T> Admission<T> {
    pub(crate) async fn receive(self) -> Result<T, ()> {
        let Self { receive, depth } = self;
        let result = match tokio::time::timeout(SNAPSHOT_TIMEOUT, receive).await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(_)) | Err(_) => Err(()),
        };
        drop(depth);
        result
    }
}

pub(crate) struct SnapshotAdmission(Admission<Arc<CompSnapshot>>);

impl SnapshotAdmission {
    pub(crate) async fn receive(self) -> Result<Arc<CompSnapshot>, ()> {
        self.0.receive().await
    }
}

pub(crate) type ControlAdmission = Admission<(u8, Arc<str>)>;

pub(crate) struct PortProtocolWiring {
    pub(crate) source: channel::Channel<PortCommand>,
    pub(crate) context: Arc<SnapshotContext>,
    pub(crate) observation_producer: ObservationProducer,
}

#[cfg(test)]
pub(crate) fn test_wiring(
    context: Arc<SnapshotContext>,
) -> (
    PortProtocolWiring,
    PortIngress,
    crossbeam_channel::Receiver<ObservationRecord>,
) {
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: context.queue_depth.clone(),
        control_order: Arc::new(AtomicU64::new(0)),
        pending_idle_order: context.pending_idle_order.clone(),
    };
    let (observation_producer, observations) =
        port_observation::outbox(Arc::clone(&context.lost_count));
    (
        PortProtocolWiring {
            source,
            context,
            observation_producer,
        },
        ingress,
        observations,
    )
}

pub(crate) struct PortStarter {
    service: String,
    noded_url: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    observations: crossbeam_channel::Receiver<ObservationRecord>,
    lost_count: Arc<AtomicU64>,
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
    let publish_timeouts = Arc::new(AtomicU64::new(0));
    let event_seq = Arc::new(AtomicU64::new(0));
    let lost_count = Arc::new(AtomicU64::new(0));
    let pending_idle_order = Arc::new(AtomicU64::new(0));
    let (observation_producer, observations) = port_observation::outbox(Arc::clone(&lost_count));
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: queue_depth.clone(),
        control_order: Arc::new(AtomicU64::new(0)),
        pending_idle_order: pending_idle_order.clone(),
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
        publish_timeouts: publish_timeouts.clone(),
        event_seq: event_seq.clone(),
        lost_count: lost_count.clone(),
        pending_idle_order,
    });
    Ok((
        PortProtocolWiring {
            source,
            context,
            observation_producer,
        },
        PortStarter {
            service,
            noded_url,
            ingress,
            broker,
            reply_timeouts,
            publish_timeouts,
            observations,
            lost_count,
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
        let publish_timeouts = self.publish_timeouts;
        let observations = self.observations;
        let lost_count = self.lost_count;
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
                    publish_timeouts,
                    observations,
                    lost_count,
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
    fn publish<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
        wire: &'a str,
    ) -> WorkerFuture<'a, Result<(), String>>;
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

    fn publish<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
        wire: &'a str,
    ) -> WorkerFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let (rc, body, _) = self
                .call_with_headers_raw("noded", "topic.publish", headers, wire)
                .await
                .map_err(|error| error.to_string())?;
            if rc == 0 {
                Ok(())
            } else {
                Err(format!("topic.publish rejected with rc {rc}: {body}"))
            }
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

#[allow(clippy::too_many_arguments)]
async fn worker_loop<F, Fut, C>(
    service: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    observations: crossbeam_channel::Receiver<ObservationRecord>,
    lost_count: Arc<AtomicU64>,
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
    let publisher_task = tokio::spawn(publisher_loop(
        Arc::clone(&client),
        Arc::from(service.as_str()),
        observations,
        Arc::clone(&lost_count),
        Arc::clone(&publish_timeouts),
        shutdown.clone(),
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
                    &service,
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
    publisher_task.abort();
    let _ = publisher_task.await;
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
    service: &str,
    command: cosmix_client::IncomingCommand,
) {
    while let Some(completed) = responders.try_join_next() {
        if let Err(error) = completed {
            tracing::debug!(%error, "compositor Bus responder task stopped");
        }
    }
    let malformed =
        !command.body.is_empty() && serde_json::from_str::<Value>(&command.body).is_err();
    if command.from == "noded"
        && matches!(command.command.as_str(), "topic.active" | "topic.idle")
        && command.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("name")
                && value
                    == &port_observation::topic_name(service, port_observation::PROPS_TOPIC_SUFFIX)
        })
    {
        ingress.set_watch_state(command.command == "topic.active");
        return;
    }
    if command.command == "comp.ping" {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, (0, Arc::from("{\"pong\":true}"))),
        );
        return;
    }
    if command.command == "comp.props.watch" {
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
        let admission = match ingress.request_watch() {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
        );
        return;
    }
    if command.command == "comp.props.set" {
        if authorize_set(&command).is_err() {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("not_local")),
            );
            return;
        }
        let parsed = if malformed {
            Err(invalid_set_shape(None))
        } else {
            parse_set(&command.args)
        };
        let (path, value) = match parsed {
            Ok(parsed) => parsed,
            Err(reply) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, reply),
                );
                return;
            }
        };
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
        let admission = match ingress.request_set(path, value) {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
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

fn authorize_set(command: &cosmix_client::IncomingCommand) -> Result<(), ()> {
    if command.from == "anonymous"
        || validate_service_name(&command.from).is_err()
        || command.headers.keys().any(|name| {
            name.eq_ignore_ascii_case("source_peer")
                || name.eq_ignore_ascii_case("permissions")
                || name.eq_ignore_ascii_case("signed_ident")
        })
    {
        return Err(());
    }
    let origins = command
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("broker_origin"))
        .collect::<Vec<_>>();
    if origins.len() == 1 && origins[0].1 == "local" {
        Ok(())
    } else {
        Err(())
    }
}

fn parse_set(args: &Value) -> Result<(String, Value), (u8, Arc<str>)> {
    let object = args.as_object().ok_or_else(|| invalid_set_shape(None))?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| error("unknown_path"))?;
    let value = object
        .get("value")
        .cloned()
        .ok_or_else(|| invalid_set_shape(Some(path)))?;
    Ok((path.to_string(), value))
}

fn invalid_set_shape(path: Option<&str>) -> (u8, Arc<str>) {
    (
        10,
        Arc::from(
            json!({
                "error": "invalid_value",
                "path": path,
                "expected": "JSON property value",
                "range": "descriptor",
            })
            .to_string(),
        ),
    )
}

fn spawn_control_responder(
    responders: &mut JoinSet<()>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    command: cosmix_client::IncomingCommand,
    admission: ControlAdmission,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let reply_sender = reply_sender.clone();
    let reply_timeouts = Arc::clone(reply_timeouts);
    responders.spawn(async move {
        let _permit = permit;
        let reply = admission.receive().await.unwrap_or_else(|()| error("busy"));
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

#[derive(Clone, Copy)]
struct PendingGap {
    last_lost_seq: u64,
    cause: &'static str,
}

async fn publisher_loop<C: WorkerClient>(
    client: Arc<C>,
    service: Arc<str>,
    observations: crossbeam_channel::Receiver<ObservationRecord>,
    lost_count: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(PUBLISH_POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_sequence = 1_u64;
    let mut pending_gap = None;
    let mut pending_record = None;
    loop {
        if pending_record.is_none() {
            pending_record = match observations.try_recv() {
                Ok(record) => Some(record),
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                break;
                            }
                        }
                        _ = interval.tick() => {}
                    }
                    continue;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            };
        }
        let Some(record) = pending_record.as_ref() else {
            continue;
        };
        let topic = port_observation::topic_name(&service, record.topic_suffix());
        if record.event_seq() > next_sequence {
            pending_gap = Some(PendingGap {
                last_lost_seq: record.event_seq().saturating_sub(1),
                cause: pending_gap.map_or("outbox.overflow", |gap: PendingGap| gap.cause),
            });
        }
        if let Some(gap) = pending_gap {
            let message = gap_message(
                record.topic_suffix(),
                gap,
                lost_count.load(Ordering::Acquire),
            );
            if publish_message(client.as_ref(), &topic, &message)
                .await
                .is_err()
            {
                publish_timeouts.fetch_add(1, Ordering::AcqRel);
                let failed = pending_record.take().expect("gap has a surviving record");
                let (discarded, last_lost_seq) = discard_publication_backlog(failed, &observations);
                lost_count.fetch_add(discarded, Ordering::AcqRel);
                next_sequence = last_lost_seq.saturating_add(1);
                pending_gap = Some(PendingGap {
                    last_lost_seq: gap.last_lost_seq.max(last_lost_seq),
                    cause: "publisher.loss",
                });
                continue;
            }
            pending_gap = None;
        }

        let record = pending_record.take().expect("checked above");
        let sequence = record.event_seq();
        let message = record.wire();
        if publish_message(client.as_ref(), &topic, &message)
            .await
            .is_ok()
        {
            next_sequence = sequence.saturating_add(1);
            continue;
        }

        publish_timeouts.fetch_add(1, Ordering::AcqRel);
        let (discarded, last_lost_seq) = discard_publication_backlog(record, &observations);
        lost_count.fetch_add(discarded, Ordering::AcqRel);
        next_sequence = last_lost_seq.saturating_add(1);
        pending_gap = Some(PendingGap {
            last_lost_seq,
            cause: "publisher.loss",
        });
    }
}

fn discard_publication_backlog(
    failed: ObservationRecord,
    observations: &crossbeam_channel::Receiver<ObservationRecord>,
) -> (u64, u64) {
    let mut discarded = 1_u64;
    let mut last_lost_seq = failed.event_seq();
    while let Ok(queued) = observations.try_recv() {
        discarded = discarded.saturating_add(1);
        last_lost_seq = last_lost_seq.max(queued.event_seq());
    }
    (discarded, last_lost_seq)
}

async fn publish_message<C: WorkerClient>(
    client: &C,
    topic: &str,
    message: &BusMessage,
) -> Result<(), ()> {
    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), topic.to_string());
    headers.insert("retain".to_string(), "false".to_string());
    let wire = message.to_wire();
    match tokio::time::timeout(PUBLISH_TIMEOUT, client.publish(&headers, &wire)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::debug!(%error, topic, "compositor Bus topic publication failed");
            Err(())
        }
        Err(_) => {
            tracing::debug!(
                topic,
                timeout_ms = PUBLISH_TIMEOUT.as_millis(),
                "compositor Bus topic publication timed out"
            );
            Err(())
        }
    }
}

fn gap_message(topic_suffix: &str, gap: PendingGap, lost_count: u64) -> BusMessage {
    let mut message = BusMessage::new()
        .with_header("command", topic_suffix)
        .with_header("event_seq", &gap.last_lost_seq.to_string());
    message.body = json!({
        "gap": true,
        "lost_count": lost_count,
        "cause": gap.cause,
    })
    .to_string();
    message
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

    type PublishedMessages = Arc<Mutex<Vec<(BTreeMap<String, String>, String)>>>;

    struct FakeClient {
        incoming: Mutex<Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>>>,
        states: watch::Sender<ConnState>,
        hang_replies: bool,
        responses_started: Arc<AtomicUsize>,
        publish_mode: Arc<AtomicU8>,
        publications: PublishedMessages,
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
                    publish_mode: Arc::new(AtomicU8::new(0)),
                    publications: Arc::new(Mutex::new(Vec::new())),
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

        fn publish<'a>(
            &'a self,
            headers: &'a BTreeMap<String, String>,
            wire: &'a str,
        ) -> WorkerFuture<'a, Result<(), String>> {
            match self.publish_mode.load(Ordering::Acquire) {
                1 => Box::pin(future::ready(Err("publication rejected".into()))),
                2 => Box::pin(future::pending()),
                _ => {
                    self.publications
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((headers.clone(), wire.to_string()));
                    Box::pin(future::ready(Ok(())))
                }
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
                control_order: Arc::new(AtomicU64::new(0)),
                pending_idle_order: Arc::new(AtomicU64::new(0)),
            },
            source,
            queue_depth,
        )
    }

    fn test_observation_args() -> (
        Arc<AtomicU64>,
        crossbeam_channel::Receiver<ObservationRecord>,
        Arc<AtomicU64>,
    ) {
        let lost = Arc::new(AtomicU64::new(0));
        let (_producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        (Arc::new(AtomicU64::new(0)), receiver, lost)
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
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            lost_count,
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
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            publish_timeouts,
            observations,
            lost_count,
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
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            publish_timeouts,
            observations,
            lost_count,
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
            "comp-nested",
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
            "comp-nested",
            get,
        );
        let reply = replies.recv().await.expect("malformed read reply queued");
        assert_eq!(reply.rc, 10);
        assert_eq!(reply.body.as_ref(), "{\"error\":\"unknown_path\"}");
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    fn local_set_command(id: usize, path: &str, value: Value) -> cosmix_client::IncomingCommand {
        let mut command = command("comp.props.set", id);
        command.args = json!({"path": path, "value": value});
        command.body = command.args.to_string();
        command
            .headers
            .insert("broker_origin".into(), "local".into());
        command
    }

    #[test]
    fn set_authorisation_requires_one_local_stamp_and_canonical_caller() {
        assert!(authorize_set(&local_set_command(1, "input.corners.enabled", json!(true))).is_ok());

        let mut missing = local_set_command(1, "input.corners.enabled", json!(true));
        missing.headers.clear();
        assert!(authorize_set(&missing).is_err());

        let mut duplicate = local_set_command(1, "input.corners.enabled", json!(true));
        duplicate
            .headers
            .insert("Broker_Origin".into(), "local".into());
        assert!(authorize_set(&duplicate).is_err());

        let mut remote = local_set_command(1, "input.corners.enabled", json!(true));
        remote.headers.insert("broker_origin".into(), "mesh".into());
        assert!(authorize_set(&remote).is_err());

        for caller in ["", "anonymous", "Bad-Caller", "a"] {
            let mut command = local_set_command(1, "input.corners.enabled", json!(true));
            command.from = caller.into();
            assert!(authorize_set(&command).is_err(), "caller {caller:?}");
        }
    }

    #[test]
    fn set_authorisation_rejects_every_wire_identity_claim() {
        for claim in ["source_peer", "permissions", "signed_ident"] {
            let mut command = local_set_command(1, "input.corners.enabled", json!(true));
            command.headers.insert(claim.into(), "forged".into());
            assert!(authorize_set(&command).is_err(), "claim {claim}");
        }
    }

    #[tokio::test]
    async fn authorised_set_crosses_ingress_and_preserves_response_correlation() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let command = local_set_command(37, "input.corners.dwell_ms", json!(250));

        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command,
        );
        let PortCommand::Set(request) = source.try_recv().expect("set admitted") else {
            panic!("set command expected");
        };
        assert_eq!(request.path, "input.corners.dwell_ms");
        assert_eq!(request.value, json!(250));
        request
            .reply
            .send((
                0,
                Arc::from("{\"path\":\"input.corners.dwell_ms\",\"old\":200,\"new\":250}"),
            ))
            .expect("responder remains live");
        responders
            .join_next()
            .await
            .expect("responder completes")
            .expect("task");
        let reply = replies.recv().await.expect("correlated reply");
        assert_eq!(reply.id.as_deref(), Some("37"));
        assert_eq!(reply.command, "comp.props.set");
        assert_eq!(reply.rc, 0);
    }

    #[tokio::test]
    async fn unauthorised_set_is_rejected_before_admission() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let mut command = local_set_command(2, "input.corners.enabled", json!(false));
        command.headers.clear();
        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command,
        );
        let reply = replies.recv().await.expect("not-local reply");
        assert_eq!(reply.rc, 10);
        assert_eq!(reply.body.as_ref(), "{\"error\":\"not_local\"}");
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn non_finite_json_set_is_rejected_before_admission() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let mut command = local_set_command(3, "input.corners.velocity_max_px_s", json!(1500.0));
        command.body = "{\"path\":\"input.corners.velocity_max_px_s\",\"value\":NaN}".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command,
        );
        let reply = replies.recv().await.expect("invalid-value reply");
        assert_eq!(reply.rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&reply.body).unwrap()["error"],
            "invalid_value"
        );
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn exact_noded_topic_lifecycle_notices_cross_as_watch_state_only() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, _replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        for (verb, active) in [("topic.active", true), ("topic.idle", false)] {
            let mut command = command(verb, 1);
            command.from = "noded".into();
            command
                .headers
                .insert("name".into(), "comp-nested.props.changed".into());
            handle_incoming(
                &ingress,
                &mut responders,
                &permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                command,
            );
            let PortCommand::WatchState {
                active: observed, ..
            } = source.try_recv().expect("notice staged")
            else {
                panic!("watch-state command expected");
            };
            assert_eq!(observed, active);
        }
    }

    #[test]
    fn idle_notice_survives_a_saturated_control_ingress() {
        let (ingress, _source, _) = test_ingress();
        let _admissions = (0..PORT_QUEUE_CAPACITY)
            .map(|_| ingress.request_snapshot().expect("fill ingress"))
            .collect::<Vec<_>>();
        ingress.set_watch_state(false);
        let idle_order = ingress.pending_idle_order.load(Ordering::Acquire);
        assert_ne!(idle_order, 0);
        let active_order = ingress.next_control_order();
        let (reply, _receive) = tokio::sync::oneshot::channel();
        let watch_order = ingress.next_control_order();
        let mut controls = [
            PortControl::Watch(PortReply {
                order: watch_order,
                reply,
            }),
            PortControl::WatchState {
                active: true,
                order: active_order,
            },
            PortControl::WatchState {
                active: false,
                order: idle_order,
            },
        ];
        controls.sort_by_key(PortControl::order);
        assert_eq!(
            controls.map(|control| control.order()),
            [idle_order, active_order, watch_order]
        );
    }

    #[tokio::test]
    async fn saturated_ingress_returns_busy_before_set_reaches_calloop() {
        let (ingress, source, _) = test_ingress();
        let _admissions = (0..PORT_QUEUE_CAPACITY)
            .map(|_| ingress.request_snapshot().expect("fill ingress"))
            .collect::<Vec<_>>();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            local_set_command(3, "input.corners.dwell_ms", json!(250)),
        );
        let reply = replies.recv().await.expect("busy reply");
        assert_eq!(reply.body.as_ref(), "{\"error\":\"busy\"}");
        for _ in 0..PORT_QUEUE_CAPACITY {
            assert!(matches!(source.try_recv(), Ok(PortCommand::Snapshot(_))));
        }
    }

    #[tokio::test]
    async fn publisher_places_outbox_gap_before_surviving_records() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        for event_seq in 1..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if publications
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len()
                    == 3
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("gap and two survivors publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");

        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gap = cosmix_bus::bus::parse(&published[0].1).expect("gap wire parses");
        assert_eq!(
            published[0].0.get("name").map(String::as_str),
            Some("comp-nested.focus.changed")
        );
        assert_eq!(gap.get("command"), Some("focus.changed"));
        assert_eq!(gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "outbox.overflow"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn rejected_publication_discards_backlog_and_recovers_with_one_gap() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(2),
            previous: Some(1),
            exclusive_latch: None,
            event_seq: 2,
        });
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(1, Ordering::Release);
        let mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while lost.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed record and backlog become loss");
        mode.store(0, Ordering::Release);
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(3),
            previous: Some(2),
            exclusive_latch: None,
            event_seq: 3,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publisher-loss gap and survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gap = cosmix_bus::bus::parse(&published[0].1).expect("gap parses");
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "publisher.loss"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn rejected_gap_discards_its_survivor_and_recovers_as_publisher_loss() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        for sequence in 1..=3 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(sequence),
                previous: None,
                exclusive_latch: None,
                event_seq: sequence,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(1, Ordering::Release);
        let mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while lost.load(Ordering::Acquire) != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed gap discards every surviving record");
        mode.store(0, Ordering::Release);
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(4),
            previous: Some(3),
            exclusive_latch: None,
            event_seq: 4,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement gap and survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gap = cosmix_bus::bus::parse(&published[0].1).expect("gap parses");
        assert_eq!(gap.get("event_seq"), Some("3"));
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 3, "cause": "publisher.loss"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_publication_times_out_without_blocking_shutdown() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: None,
            previous: Some(1),
            exclusive_latch: None,
            event_seq: 1,
        });
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(2, Ordering::Release);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(PUBLISH_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
        assert_eq!(lost.load(Ordering::Acquire), 1);
        shutdown_tx.send_replace(true);
        tokio::time::timeout(Duration::from_millis(1), task)
            .await
            .expect("publisher shutdown is bounded")
            .expect("publisher exits");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_publisher_does_not_block_commands_and_worker_shutdown_is_bounded() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, observations) = port_observation::outbox(Arc::clone(&lost));
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        });
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(2, Ordering::Release);
        let responses_started = Arc::clone(&client.responses_started);
        let mut client = Some(client);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            lost,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;
        commands.send(command("comp.ping", 1)).expect("worker live");
        wait_for_counter(&responses_started, 1, "ping replied while publish hangs").await;
        shutdown_tx.send_replace(true);
        tokio::time::timeout(Duration::from_millis(1), worker)
            .await
            .expect("worker shutdown does not await publisher deadline")
            .expect("worker exits");
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
            let PortCommand::Snapshot(request) = source.try_recv().expect("staged request") else {
                panic!("snapshot request expected");
            };
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
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            lost_count,
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
            let PortCommand::Snapshot(request) = next_port_command(&source).await else {
                panic!("snapshot request expected");
            };
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
        let PortCommand::Snapshot(request) = next_port_command(&source).await else {
            panic!("snapshot request expected");
        };
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
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::clone(&reply_timeouts),
            publish_timeouts,
            observations,
            lost_count,
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
        let PortCommand::Snapshot(request) = next_port_command(&source).await else {
            panic!("snapshot request expected");
        };
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
