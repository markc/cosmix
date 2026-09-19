//! Bus citizen for the `dbusd` control service: supervision verbs,
//! `dbusd.props.*`, and state-change events. Also owns the built-in
//! adapter registry.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_props_core::PropTree;
use cosmix_props_core::PropValue;
use cosmix_props_core::publish::{build_props_changed_message, props_changed_topic};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::adapter::{AdapterSpec, SessionBus};
use crate::state::AdapterEvent;
use crate::supervisor::{LifecycleCmd, StartedSupervisor, SupervisorHandle};

pub const BUS_SERVICE: &str = "dbusd";
pub const TOPIC_ADAPTER_CHANGED: &str = "dbusd.adapter.changed";

const BROKER_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long serve() waits for the supervision tasks to wind down after
/// signalling shutdown (each already grants every run its own
/// [`crate::supervisor::GRACEFUL_STOP`] window).
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(15);

/// The built-in adapter registry. J1 ships the host only — adding an
/// adapter later is one line here:
///
/// ```ignore
/// adapter_spec::<NotifyAdapter>(),
/// ```
pub fn builtin_adapters() -> Vec<AdapterSpec> {
    Vec::new()
}

type LifecycleDone = (String, std::result::Result<(), tokio::task::JoinError>);

/// Start supervision, the event publisher and the reconnecting `dbusd`
/// Bus citizen; serve until SIGTERM/SIGINT. Adapter faults never take
/// this down — only the shutdown signals, or a supervision-task death
/// (a daemon-level bug), end it. SIGTERM exits 0 after every adapter
/// released its names.
pub async fn serve() -> Result<()> {
    let settings = crate::config::load_settings();
    let specs = builtin_adapters();
    let builtin: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
    let (enabled, unknown) = crate::config::resolve_enabled(settings.enabled.as_deref(), &builtin);
    for name in unknown {
        eprintln!("cosmix-dbusd: dbusd.conf.mix enables unknown adapter '{name}'; ignored");
    }
    let session_bus = SessionBus::from_env();

    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let StartedSupervisor {
        handle,
        events,
        lifecycles,
    } = crate::supervisor::start(
        specs
            .into_iter()
            .map(|spec| {
                let enabled = enabled.contains(&spec.name);
                (spec, enabled)
            })
            .collect(),
        session_bus.clone(),
        shutdown_rx.clone(),
        Some(crate::adapter::BusIdentity {
            service: String::new(),
            provenance: provenance.clone(),
        }),
    );

    let (client_tx, client_rx) = watch::channel::<Option<Arc<NodedClient>>>(None);
    let (publisher_fault_tx, publisher_fault_rx) = mpsc::channel(1);
    let mut publisher = tokio::spawn(run_publisher(
        handle.clone(),
        events,
        client_rx,
        publisher_fault_tx,
    ));
    let mut broker = tokio::spawn(run_broker(
        handle.clone(),
        session_bus.clone(),
        client_tx,
        publisher_fault_rx,
        provenance,
        shutdown_rx.clone(),
    ));

    let mut finished: futures_util::stream::FuturesUnordered<
        Pin<Box<dyn Future<Output = LifecycleDone> + Send>>,
    > = futures_util::stream::FuturesUnordered::new();
    for (name, join) in lifecycles {
        finished.push(Box::pin(async move { (name, join.await) }));
    }

    enum Exit {
        Signal(Result<()>),
        Lifecycle(String, std::result::Result<(), tokio::task::JoinError>),
        Publisher(std::result::Result<Result<()>, tokio::task::JoinError>),
        Broker(std::result::Result<Result<()>, tokio::task::JoinError>),
    }

    let exit = tokio::select! {
        signal = shutdown_signal() => Exit::Signal(signal),
        Some((name, result)) = finished.next() => Exit::Lifecycle(name, result),
        result = &mut publisher => Exit::Publisher(result),
        result = &mut broker => Exit::Broker(result),
    };

    let _ = shutdown_tx.send(true);
    let graceful = matches!(exit, Exit::Signal(Ok(())));
    if graceful && !broker.is_finished() {
        match broker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("cosmix-dbusd: broker shutdown failed; continuing teardown: {error:#}")
            }
            Err(error) => eprintln!(
                "cosmix-dbusd: broker task failed during shutdown; continuing teardown: {error}"
            ),
        }
    } else {
        broker.abort();
    }
    publisher.abort();
    // Supervision tasks stop their runs, dropping the adapters' Bus and
    // zbus connections (names released), and return; drain them rather
    // than leaving that work half-done at process exit.
    if tokio::time::timeout(SHUTDOWN_DRAIN, finished.count())
        .await
        .is_err()
    {
        eprintln!("cosmix-dbusd: supervision tasks did not stop within {SHUTDOWN_DRAIN:?}");
    }

    match exit {
        Exit::Signal(result) => result,
        Exit::Lifecycle(name, Ok(())) => {
            eprintln!("cosmix-dbusd: supervision for adapter '{name}' ended unexpectedly");
            Err(anyhow!(
                "supervision for adapter '{name}' ended unexpectedly"
            ))
        }
        Exit::Lifecycle(name, Err(error)) => {
            eprintln!("cosmix-dbusd: supervision for adapter '{name}' failed: {error}");
            Err(anyhow!("supervision for adapter '{name}' failed: {error}"))
        }
        Exit::Publisher(Ok(Ok(()))) => {
            eprintln!("cosmix-dbusd: event publisher ended unexpectedly");
            Err(anyhow!("event publisher ended unexpectedly"))
        }
        Exit::Publisher(Ok(Err(error))) => {
            eprintln!("cosmix-dbusd: event publisher failed: {error:#}");
            Err(anyhow!("event publisher failed: {error:#}"))
        }
        Exit::Publisher(Err(error)) => {
            eprintln!("cosmix-dbusd: event publisher task failed: {error}");
            Err(anyhow!("event publisher task failed: {error}"))
        }
        Exit::Broker(Ok(Ok(()))) => {
            eprintln!("cosmix-dbusd: broker loop ended before shutdown");
            Err(anyhow!("broker loop ended before shutdown"))
        }
        Exit::Broker(Ok(Err(error))) => {
            eprintln!("cosmix-dbusd: broker loop failed: {error:#}");
            Err(anyhow!("broker loop failed: {error:#}"))
        }
        Exit::Broker(Err(error)) => {
            eprintln!("cosmix-dbusd: broker task failed: {error}");
            Err(anyhow!("broker task failed: {error}"))
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("listen for SIGINT"),
        signal = terminate.recv() => signal
            .ok_or_else(|| anyhow!("SIGTERM stream ended"))
            .map(|_| ()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await.context("listen for Ctrl-C")
}

async fn run_broker(
    handle: SupervisorHandle,
    session_bus: SessionBus,
    client_tx: watch::Sender<Option<Arc<NodedClient>>>,
    mut publisher_fault_rx: mpsc::Receiver<()>,
    provenance: cosmix_bus::RegisterProvenance,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        let connection = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            connection = tokio::time::timeout(
                PUBLISH_TIMEOUT,
                cosmix_config::client_helpers::connect_default_with_provenance(
                    BUS_SERVICE,
                    provenance.clone(),
                ),
            ) => connection,
        };
        match connection {
            Ok(Ok(client)) => {
                let client = Arc::new(client);
                let _ = client_tx.send(Some(Arc::clone(&client)));
                eprintln!("cosmix-dbusd: registered as '{BUS_SERVICE}'");
                let stopping = tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                        true
                    }
                    _ = serve_bus(Arc::clone(&client), &handle, &session_bus) => false,
                    fault = publisher_fault_rx.recv() => {
                        if fault.is_none() {
                            return Err(anyhow!("publisher task ended unexpectedly"));
                        }
                        eprintln!("cosmix-dbusd: publisher fault; reconnecting broker client");
                        false
                    }
                };
                let _ = client_tx.send(None);
                if tokio::time::timeout(PUBLISH_TIMEOUT, client.close())
                    .await
                    .is_err()
                {
                    eprintln!("cosmix-dbusd: broker client close timed out");
                }
                while publisher_fault_rx.try_recv().is_ok() {}
                if stopping {
                    return Ok(());
                }
                eprintln!("cosmix-dbusd: broker disconnected; retrying in 60s");
            }
            Ok(Err(error)) => {
                eprintln!("cosmix-dbusd: broker unavailable; retrying in 60s: {error}");
            }
            Err(_) => {
                eprintln!("cosmix-dbusd: broker connection timed out; retrying in 60s");
            }
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            _ = tokio::time::sleep(BROKER_RECONNECT_DELAY) => {}
        }
    }
}

async fn serve_bus(client: Arc<NodedClient>, handle: &SupervisorHandle, session_bus: &SessionBus) {
    let Some(mut incoming) = client.incoming_async().await else {
        return;
    };
    while let Some(command) = incoming.recv().await {
        let (rc, body) = dispatch(&command, handle, session_bus);
        match tokio::time::timeout(
            PUBLISH_TIMEOUT,
            client.respond_parts(
                &command.from,
                &command.command,
                command.id.as_deref(),
                rc,
                &body,
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("cosmix-dbusd: Bus response failed; reconnecting: {error}");
                return;
            }
            Err(_) => {
                eprintln!("cosmix-dbusd: Bus response timed out; reconnecting");
                return;
            }
        }
    }
}

/// Synchronous dispatch for one incoming command. Mesh-open per the LAW
/// 2026-09-15: no caller authorization on any verb. Unknown verbs and
/// unknown adapter names are refusals (rc 10 + error body), never
/// panics.
fn dispatch(
    command: &IncomingCommand,
    handle: &SupervisorHandle,
    session_bus: &SessionBus,
) -> (u8, String) {
    if let Some(suffix) = command.command.strip_prefix("dbusd.props.") {
        if suffix == "watch" {
            return (
                0,
                json!({
                    "topic": props_changed_topic(BUS_SERVICE),
                    "domain_topics": [TOPIC_ADAPTER_CHANGED],
                    "event_sequence": "daemon_session_monotonic",
                    "bootstrap": "subscribe on this connection, then read dbusd.props.get",
                })
                .to_string(),
            );
        }
        let props = crate::props::DbusdProps::new(&handle.statuses());
        let args = resolve_args(command);
        let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
        return (response.rc.clamp(0, 255) as u8, response.body);
    }

    match command.command.as_str() {
        "dbusd.adapters" => {
            let statuses = handle.statuses();
            (
                0,
                json!({
                    "adapters": statuses.iter().map(status_json).collect::<Vec<_>>(),
                    "session_bus": session_bus_json(session_bus),
                })
                .to_string(),
            )
        }
        "dbusd.adapter.restart" => control_adapter(command, handle, LifecycleCmd::Restart),
        "dbusd.adapter.enable" => control_adapter(command, handle, LifecycleCmd::Enable),
        "dbusd.adapter.disable" => control_adapter(command, handle, LifecycleCmd::Disable),
        "dbusd.ping" => (
            0,
            json!({"pong": true, "service": BUS_SERVICE, "schema": "dbusd.v1"}).to_string(),
        ),
        "dbusd.info" => {
            let build = cosmix_buildinfo::build_info!();
            (
                0,
                json!({
                    "name": BUS_SERVICE,
                    "schema": "dbusd.v1",
                    "props_level": "L2",
                    "binary": build.pkg,
                    "version": build.version,
                    "git_sha": build.git_sha,
                    "git_dirty": build.git_dirty,
                    "build_time": build.build_time,
                    "adapters": handle.statuses().len(),
                    "session_bus": session_bus_json(session_bus),
                })
                .to_string(),
            )
        }
        _ => (
            10,
            json!({"error": format!("unknown dbusd verb: {}", command.command)}).to_string(),
        ),
    }
}

fn control_adapter(
    command: &IncomingCommand,
    handle: &SupervisorHandle,
    cmd: LifecycleCmd,
) -> (u8, String) {
    let action = match cmd {
        LifecycleCmd::Restart => "restart",
        LifecycleCmd::Enable => "enable",
        LifecycleCmd::Disable => "disable",
    };
    let args = resolve_args(command);
    let Some(name) = args
        .as_ref()
        .and_then(|args| args.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return (
            10,
            json!({"error": format!("dbusd.adapter.{action} requires args.name")}).to_string(),
        );
    };
    match handle.control(&name, cmd) {
        Ok(()) => (
            0,
            json!({"ok": true, "action": action, "name": name}).to_string(),
        ),
        Err(error) => (10, json!({"error": error, "name": name}).to_string()),
    }
}

fn status_json(status: &crate::state::AdapterStatus) -> Value {
    json!({
        "name": status.name,
        "service": status.service,
        "state": status.state.as_str(),
        "restarts": status.restarts,
        "last_error": status.last_error,
        "since": rfc3339(status.since),
    })
}

fn session_bus_json(session_bus: &SessionBus) -> Value {
    match session_bus {
        SessionBus::Address(address) => json!({"available": true, "address": address}),
        SessionBus::Unavailable(reason) => json!({"available": false, "reason": reason}),
    }
}

fn rfc3339(moment: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(moment)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Consume supervision events and publish them on the daemon's `dbusd`
/// connection: `dbusd.props.changed` diffs plus one
/// `dbusd.adapter.changed` event per change. No polling — every
/// publication is driven by an event. A publish failure faults the
/// broker client (reconnect) instead of losing supervision.
async fn run_publisher(
    handle: SupervisorHandle,
    mut events: mpsc::Receiver<AdapterEvent>,
    mut clients: watch::Receiver<Option<Arc<NodedClient>>>,
    faults: mpsc::Sender<()>,
) -> Result<()> {
    // Diff baseline for the props stream. Reset whenever publishing
    // fails so a fresh connection does not diff against a snapshot it
    // never served.
    let mut last_props: Option<PropValue> = None;
    loop {
        let Some(event) = events.recv().await else {
            return Err(anyhow!("supervision event stream ended"));
        };
        let client = match wait_for_client(&mut clients).await {
            Ok(client) => client,
            Err(error) => return Err(error),
        };

        let statuses = handle.statuses();
        let snapshot = crate::props::DbusdProps::new(&statuses).snapshot();
        let mut sent = publish_event_diffs(&client, last_props.as_ref(), &snapshot).await;
        if sent.is_ok() {
            sent = publish(
                &client,
                TOPIC_ADAPTER_CHANGED,
                adapter_changed_message(&event),
            )
            .await;
        }
        match sent {
            Ok(()) => last_props = Some(snapshot),
            Err(error) => {
                eprintln!("cosmix-dbusd: event publish failed: {error:#}");
                last_props = None;
                let _ = faults.try_send(());
            }
        }
    }
}

async fn wait_for_client(
    clients: &mut watch::Receiver<Option<Arc<NodedClient>>>,
) -> Result<Arc<NodedClient>> {
    loop {
        if let Some(client) = clients.borrow_and_update().clone() {
            return Ok(client);
        }
        // No Bus connection yet: park until one appears. Events buffer
        // in the bounded channel meanwhile; overflow is dropped at the
        // supervisor with a log line, and props.get is the bootstrap.
        if clients.changed().await.is_err() {
            return Err(anyhow!("publisher client channel ended"));
        }
    }
}

async fn publish_event_diffs(
    client: &NodedClient,
    old: Option<&PropValue>,
    new: &PropValue,
) -> Result<()> {
    let Some(old) = old else {
        return Ok(());
    };
    for (path, old_value, new_value) in cosmix_props_core::diff(old, new) {
        let message =
            build_props_changed_message(&path, &old_value, &new_value, "supervisor.state_change");
        publish(client, &props_changed_topic(BUS_SERVICE), message).await?;
    }
    Ok(())
}

fn adapter_changed_message(event: &AdapterEvent) -> cosmix_bus::bus::BusMessage {
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", "adapter.changed");
    message.body = json!({
        "event": "adapter.changed",
        "data": {
            "name": event.status.name,
            "service": event.status.service,
            "state": event.status.state.as_str(),
            "previous": event.previous.as_str(),
            "restarts": event.status.restarts,
            "last_error": event.status.last_error,
            "since": rfc3339(event.status.since),
        }
    })
    .to_string();
    message
}

async fn publish(
    client: &NodedClient,
    topic: &str,
    message: cosmix_bus::bus::BusMessage,
) -> Result<()> {
    let headers = BTreeMap::from([
        ("name".to_string(), topic.to_string()),
        ("retain".to_string(), "false".to_string()),
    ]);
    let wire = message.to_wire();
    client
        .send_with_headers("noded", "topic.publish", &headers, &wire)
        .await
}

fn resolve_args(command: &IncomingCommand) -> Option<Value> {
    if let Some(args) = command.header("args")
        && let Ok(value) = serde_json::from_str(args)
    {
        return Some(value);
    }
    if !command.args.is_null() {
        return Some(command.args.clone());
    }
    if !command.body.is_empty()
        && let Ok(value) = serde_json::from_str(&command.body)
    {
        return Some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AdapterFactory;
    use crate::state::AdapterStateKind;
    use std::time::SystemTime;

    fn command(verb: &str, args: Value) -> IncomingCommand {
        IncomingCommand {
            from: "alpha".into(),
            command: verb.into(),
            id: Some("1".into()),
            args,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    /// A supervisor with one adapter that starts DISABLED, so its state
    /// is deterministically `disabled` (no launch races the assertions)
    /// and its lifecycle task — with a live control channel — exists
    /// for the verb tests.
    async fn test_handle() -> SupervisorHandle {
        let factory: AdapterFactory = Arc::new(|| Box::new(crate::fault::FaultAdapter::default()));
        let spec = AdapterSpec {
            name: "notify".into(),
            service: "notify".into(),
            factory,
        };
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        crate::supervisor::start(
            vec![(spec, false)],
            SessionBus::Unavailable("unset".into()),
            shutdown_rx,
            None,
        )
        .handle
    }

    #[tokio::test]
    async fn adapters_lists_supervision_rows() {
        let handle = test_handle().await;
        let (rc, body) = dispatch(
            &command("dbusd.adapters", Value::Null),
            &handle,
            &SessionBus::Address("unix:path=/run/bus".into()),
        );
        assert_eq!(rc, 0);
        let body: Value = serde_json::from_str(&body).unwrap();
        let adapters = body["adapters"].as_array().unwrap();
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0]["name"], "notify");
        assert_eq!(adapters[0]["state"], "disabled");
        assert_eq!(adapters[0]["restarts"], 0);
        assert!(adapters[0]["since"].as_str().is_some());
        assert_eq!(body["session_bus"]["address"], "unix:path=/run/bus");
    }

    #[tokio::test]
    async fn unknown_adapter_control_is_a_refusal_not_a_panic() {
        let handle = test_handle().await;
        for verb in [
            "dbusd.adapter.restart",
            "dbusd.adapter.enable",
            "dbusd.adapter.disable",
        ] {
            let (rc, body) = dispatch(
                &command(verb, json!({"name": "nonsense"})),
                &handle,
                &SessionBus::Unavailable("unset".into()),
            );
            assert_eq!(rc, 10, "{verb}");
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"], "unknown adapter: nonsense");
        }

        // Known name is accepted (the disabled lifecycle wakes up).
        let (rc, body) = dispatch(
            &command("dbusd.adapter.enable", json!({"name": "notify"})),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["action"], "enable");
    }

    #[tokio::test]
    async fn control_without_name_is_refused() {
        let handle = test_handle().await;
        let (rc, body) = dispatch(
            &command("dbusd.adapter.restart", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 10);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("requires args.name")
        );
    }

    #[tokio::test]
    async fn unknown_verb_is_a_refusal() {
        let handle = test_handle().await;
        let (rc, body) = dispatch(
            &command("dbusd.frobnicate", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 10);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("unknown dbusd verb")
        );
    }

    #[tokio::test]
    async fn props_surface_is_dispatchable() {
        let handle = test_handle().await;
        let (rc, list) = dispatch(
            &command("dbusd.props.list", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        let list: Value = serde_json::from_str(&list).unwrap();
        assert!(
            list.as_array()
                .unwrap()
                .contains(&json!("adapters.notify.state"))
        );

        let (rc, get) = dispatch(
            &command("dbusd.props.get", json!({"path": "adapters.notify.state"})),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&get).unwrap(),
            json!("disabled")
        );

        let (rc, watch_body) = dispatch(
            &command("dbusd.props.watch", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        let watch_body: Value = serde_json::from_str(&watch_body).unwrap();
        assert_eq!(watch_body["topic"], "dbusd.props.changed");
        assert_eq!(watch_body["domain_topics"][0], "dbusd.adapter.changed");
    }

    #[test]
    fn adapter_changed_message_carries_the_new_state() {
        let event = AdapterEvent {
            status: crate::state::AdapterStatus {
                name: "tray".into(),
                service: "tray".into(),
                state: AdapterStateKind::Backoff,
                restarts: 3,
                last_error: Some("panicked: boom".into()),
                since: SystemTime::UNIX_EPOCH,
            },
            previous: AdapterStateKind::Starting,
        };
        let message = adapter_changed_message(&event);
        assert_eq!(message.get("command"), Some("adapter.changed"));
        let body: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(body["data"]["state"], "backoff");
        assert_eq!(body["data"]["previous"], "starting");
        assert_eq!(body["data"]["restarts"], 3);
        assert_eq!(body["data"]["last_error"], "panicked: boom");
    }
}
