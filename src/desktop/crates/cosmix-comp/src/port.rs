//! `comp.*` Bus citizen worker and its bounded protocol ingress.

use std::{
    fmt::Write as _,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cosmix_client::{ConnState, NameCollision, SupervisedClient, SupervisedError};
use serde_json::Value;
use smithay::reexports::calloop::channel;
use tokio::{sync::watch, task::JoinSet};

use crate::{decoration::DecorationStartup, protocol::port_snapshot};
use port_snapshot::{
    BROKER_CONNECTED, BROKER_RETRYING, CompSnapshot, SnapshotContext, dispatch_read, error,
};

const PORT_QUEUE_CAPACITY: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
const PORT_SHUTDOWN_GRACE: Duration = Duration::from_millis(300);

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
    pub(crate) fn request_snapshot(
        &self,
    ) -> Result<tokio::sync::oneshot::Receiver<Arc<CompSnapshot>>, ()> {
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
            Ok(()) => Ok(receive),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.queue_depth.fetch_sub(1, Ordering::AcqRel);
                Err(())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn release_test_admission(&self) {
        self.queue_depth.fetch_sub(1, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(crate) fn depth_for_test(&self) -> usize {
        self.queue_depth.load(Ordering::Acquire)
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
    });
    Ok((
        PortProtocolWiring { source, context },
        PortStarter {
            service,
            noded_url,
            ingress,
            broker,
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
                runtime.block_on(worker_loop(
                    service,
                    noded_url,
                    thread_ingress,
                    broker,
                    shutdown_rx,
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
    NameCollision(String),
}

enum ConnectOutcome<C> {
    Connected(C),
    Collision(String),
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
            Err(ConnectAttemptError::NameCollision(service)) => {
                return ConnectOutcome::Collision(service);
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

async fn worker_loop(
    service: String,
    noded_url: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    mut shutdown: watch::Receiver<bool>,
) {
    let connect_service = service.clone();
    let connect_url = noded_url.clone();
    let outcome = connect_loop(
        &mut shutdown,
        &broker,
        move || {
            let service = connect_service.clone();
            let url = connect_url.clone();
            async move {
                SupervisedClient::connect_supervised(&service, &url)
                    .await
                    .map_err(|error| classify_connect_error(&service, error))
            }
        },
        Duration::ZERO,
    )
    .await;
    let client = match outcome {
        ConnectOutcome::Connected(client) => Arc::new(client),
        ConnectOutcome::Collision(collided) => {
            tracing::error!(service = %collided, "Bus service name is already registered; compositor continues without a port");
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

    loop {
        tokio::select! {
            biased;
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
                    tracing::error!(service = %service, "Bus service name collided during reconnect; compositor continues without a port");
                    break;
                }
            }
            command = incoming.recv() => {
                let Some(command) = command else {
                    let state = client.state();
                    apply_connection_state(&broker, state);
                    if state == ConnState::Fatal {
                        tracing::error!(service = %service, "Bus service name collided during reconnect; compositor continues without a port");
                    }
                    break;
                };
                handle_incoming(&client, &ingress, &mut responders, command).await;
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
    let _ = tokio::time::timeout(Duration::from_millis(250), client.shutdown()).await;
}

fn classify_connect_error(service: &str, error: SupervisedError) -> ConnectAttemptError {
    if let SupervisedError::InitialConnectFailed { source, .. } = &error
        && source.downcast_ref::<NameCollision>().is_some()
    {
        ConnectAttemptError::NameCollision(service.to_string())
    } else {
        ConnectAttemptError::Retry(error.to_string())
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

async fn handle_incoming(
    client: &Arc<SupervisedClient>,
    ingress: &PortIngress,
    responders: &mut JoinSet<()>,
    command: cosmix_client::IncomingCommand,
) {
    let malformed =
        !command.body.is_empty() && serde_json::from_str::<Value>(&command.body).is_err();
    if command.command == "comp.ping" && !malformed {
        respond(client, &command, (0, "{\"pong\":true}".to_string())).await;
        return;
    }
    let needs_snapshot = matches!(
        command.command.as_str(),
        "comp.info" | "comp.props.get" | "comp.props.list" | "comp.props.describe"
    );
    if !needs_snapshot {
        respond(client, &command, error("unknown_verb")).await;
        return;
    }
    if malformed {
        respond(client, &command, error("unknown_path")).await;
        return;
    }
    if responders.len() >= PORT_QUEUE_CAPACITY {
        respond(client, &command, error("busy")).await;
        return;
    }
    let receive = match ingress.request_snapshot() {
        Ok(receive) => receive,
        Err(()) => {
            respond(client, &command, error("busy")).await;
            return;
        }
    };
    let depth = ingress.queue_depth.clone();
    let client = Arc::clone(client);
    responders.spawn(async move {
        let _depth = QueueDepthGuard(depth);
        let reply = match tokio::time::timeout(SNAPSHOT_TIMEOUT, receive).await {
            Ok(Ok(snapshot)) => dispatch_read(&snapshot, &command.command, &command.args),
            Ok(Err(_)) | Err(_) => error("busy"),
        };
        respond(&client, &command, reply).await;
    });
}

async fn respond(
    client: &SupervisedClient,
    command: &cosmix_client::IncomingCommand,
    (rc, body): (u8, String),
) {
    if let Err(error) = client
        .respond_parts(
            &command.from,
            &command.command,
            command.id.as_deref(),
            rc,
            &body,
        )
        .await
    {
        tracing::debug!(%error, command = %command.command, "compositor Bus reply failed");
    }
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
    use std::sync::atomic::AtomicUsize;

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
    async fn worker_state_edges_update_broker_without_polling() {
        let broker = AtomicU8::new(BROKER_RETRYING);
        let (states, mut receiver) = watch::channel(ConnState::Disconnected);
        states.send_replace(ConnState::Connected);
        receiver.changed().await.expect("watch remains open");
        apply_connection_state(&broker, *receiver.borrow_and_update());
        assert_eq!(broker.load(Ordering::Acquire), BROKER_CONNECTED);
        states.send_replace(ConnState::Disconnected);
        receiver.changed().await.expect("watch remains open");
        apply_connection_state(&broker, *receiver.borrow_and_update());
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        states.send_replace(ConnState::Connected);
        receiver.changed().await.expect("watch remains open");
        apply_connection_state(&broker, *receiver.borrow_and_update());
        assert_eq!(broker.load(Ordering::Acquire), BROKER_CONNECTED);
    }

    #[tokio::test]
    async fn worker_typed_collision_exits_once_without_renaming() {
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        let broker = AtomicU8::new(BROKER_CONNECTED);
        let attempts = AtomicUsize::new(0);
        let outcome = connect_loop(
            &mut shutdown,
            &broker,
            || {
                attempts.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Err::<(), _>(ConnectAttemptError::NameCollision(
                    "comp-nested".into(),
                )))
            },
            Duration::ZERO,
        )
        .await;
        assert!(matches!(outcome, ConnectOutcome::Collision(ref name) if name == "comp-nested"));
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
    fn bounded_ingress_accepts_sixteen_rejects_the_seventeenth_and_releases_depth() {
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
        let ingress = PortIngress {
            sender,
            queue_depth: queue_depth.clone(),
        };
        let mut receives = Vec::new();
        for _ in 0..PORT_QUEUE_CAPACITY {
            receives.push(ingress.request_snapshot().expect("request admitted"));
        }
        assert!(ingress.request_snapshot().is_err());
        assert_eq!(queue_depth.load(Ordering::Acquire), PORT_QUEUE_CAPACITY);

        for _ in 0..PORT_QUEUE_CAPACITY {
            let PortCommand::Snapshot(request) = source.try_recv().expect("staged request");
            drop(request);
            drop(QueueDepthGuard(queue_depth.clone()));
        }
        for receive in receives {
            assert!(receive.blocking_recv().is_err());
        }
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);
    }
}
