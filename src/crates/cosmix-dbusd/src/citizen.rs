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

use crate::adapter::{AdapterSpec, SessionBus, adapter_spec};
use crate::state::AdapterEvent;
use crate::supervisor::{
    ABORT_STOP, GRACEFUL_STOP, LifecycleCmd, StartedSupervisor, SupervisorHandle,
};

pub const BUS_SERVICE: &str = "dbusd";
pub const TOPIC_ADAPTER_CHANGED: &str = "dbusd.adapter.changed";

const BROKER_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
/// Budget for the broker closing its Bus client at shutdown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long serve() waits for the broker task (register→drain→close)
/// after shutdown is signalled. Sits above [`CLOSE_TIMEOUT`] so a
/// healthy broker finishes its close inside it.
const BROKER_DRAIN: Duration = Duration::from_secs(35);
/// How long serve() waits for the supervision tasks to wind down after
/// signalling shutdown (each already grants every run its own
/// [`crate::supervisor::GRACEFUL_STOP`] window).
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(15);
// Wiring check, at compile time: the supervision drain must cover a
// full stop window (GRACEFUL_STOP + ABORT_STOP per run) with margin —
// asserted at two windows, so bumping either timer can't silently
// overrun SHUTDOWN_DRAIN. The unit's `TimeoutStopSec=75 s` stays the
// outer bound (see the budget note below).
const _: () = assert!(
    SHUTDOWN_DRAIN.as_secs() >= 2 * (GRACEFUL_STOP.as_secs() + ABORT_STOP.as_secs()),
    "SHUTDOWN_DRAIN must cover two full stop windows (GRACEFUL_STOP + ABORT_STOP)"
);
// Explicit shutdown budget, against the unit's `TimeoutStopSec=75`:
// broker drain <= BROKER_DRAIN 35 s (its close alone <= CLOSE_TIMEOUT
// 30 s) + supervision drain <= SHUTDOWN_DRAIN 15 s = 50 s to the end
// of serve(), plus main's 5 s runtime teardown (RUNTIME_STOP in
// main.rs) — worst case ~55 s, with margin, never a sum that just
// touches the unit limit.

/// The built-in adapter registry. Adding an adapter is one line here
/// (the `notify` domain ships; more land in later jobs).
pub fn builtin_adapters() -> Vec<AdapterSpec> {
    vec![adapter_spec::<crate::adapters::notify::NotifyAdapter>()]
}

type LifecycleDone = (String, std::result::Result<(), tokio::task::JoinError>);

/// Start supervision, the event publisher and the reconnecting `dbusd`
/// Bus citizen; serve until SIGTERM/SIGINT. Adapter faults never take
/// this down — only the shutdown signals, or a supervision-task death
/// (a daemon-level bug), end it. SIGTERM exits 0 after every adapter
/// released its names.
pub async fn serve() -> Result<()> {
    let settings = crate::config::load_settings()?;
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
        // Zero adapters (J1's registry) would otherwise close the event
        // stream the moment start returns; held until serve() ends.
        _events_keepalive,
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

    let (client_tx, client_rx) = watch::channel::<Option<Arc<dyn EventPublisher>>>(None);
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
        // Bounded: the broker's own work is register-drain plus a close
        // inside CLOSE_TIMEOUT; anything past BROKER_DRAIN is aborted so
        // shutdown stays inside its budget (RUNTIME_STOP in
        // main.rs closes it out).
        match tokio::time::timeout(BROKER_DRAIN, &mut broker).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                eprintln!("cosmix-dbusd: broker shutdown failed; continuing teardown: {error:#}")
            }
            Ok(Err(error)) => eprintln!(
                "cosmix-dbusd: broker task failed during shutdown; continuing teardown: {error}"
            ),
            Err(_) => {
                broker.abort();
                eprintln!("cosmix-dbusd: broker did not stop within {BROKER_DRAIN:?}; aborted");
            }
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

/// The one Bus operation the event publisher needs, as a trait so the
/// publisher's reconnect policy is testable without a live noded.
/// `NodedClient` is the production implementation; tests inject a fake.
trait EventPublisher: Send + Sync {
    /// Publish one message on a topic via the broker.
    fn publish_event(
        &self,
        topic: &str,
        message: cosmix_bus::bus::BusMessage,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

impl EventPublisher for NodedClient {
    fn publish_event(
        &self,
        topic: &str,
        message: cosmix_bus::bus::BusMessage,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        // Own the topic before the async block so the returned future
        // borrows only `self` (the single elided `'_` lifetime).
        let topic = topic.to_string();
        Box::pin(async move {
            let headers = BTreeMap::from([
                ("name".to_string(), topic),
                ("retain".to_string(), "false".to_string()),
            ]);
            let wire = message.to_wire();
            self.send_with_headers("noded", "topic.publish", &headers, &wire)
                .await
        })
    }
}

async fn run_broker(
    handle: SupervisorHandle,
    session_bus: SessionBus,
    client_tx: watch::Sender<Option<Arc<dyn EventPublisher>>>,
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
                let _ = client_tx.send(Some(client.clone()));
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
                if tokio::time::timeout(CLOSE_TIMEOUT, client.close())
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
                    "event_sequence": "per-daemon-session monotonic event_seq on every event; \
                                       a gap means events were dropped — re-read dbusd.props.get",
                    "event_seq": handle.event_seq(),
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
                    "event_seq": handle.event_seq(),
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
        "leaked_runs": status.leaked_runs,
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
/// `dbusd.adapter.changed` event per change, both stamped with the
/// event's per-session `event_seq` so subscribers can detect drops. No
/// polling — every publication is driven by an event. A publish failure
/// faults the broker client (reconnect) instead of losing supervision.
async fn run_publisher(
    handle: SupervisorHandle,
    mut events: mpsc::Receiver<AdapterEvent>,
    mut clients: watch::Receiver<Option<Arc<dyn EventPublisher>>>,
    faults: mpsc::Sender<()>,
) -> Result<()> {
    // Diff baseline for the props stream. Deliberately KEPT across a
    // publish failure and the publisher's own reconnect: Bus
    // subscribers subscribe to topics, not to this daemon's connection,
    // so a subscriber that stayed connected through the outage has seen
    // exactly up to the baseline and needs the accumulated diff on the
    // next event. A re-sent diff is idempotent (it carries old and new
    // values); a dropped one is a silent gap. Fresh subscribers
    // bootstrap with dbusd.props.get regardless.
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
        let mut sent =
            publish_event_diffs(&*client, last_props.as_ref(), &snapshot, event.seq).await;
        if sent.is_ok() {
            sent = client
                .publish_event(TOPIC_ADAPTER_CHANGED, adapter_changed_message(&event))
                .await;
        }
        match sent {
            Ok(()) => last_props = Some(snapshot),
            Err(error) => {
                eprintln!("cosmix-dbusd: event publish failed: {error:#}");
                let _ = faults.try_send(());
            }
        }
    }
}

async fn wait_for_client(
    clients: &mut watch::Receiver<Option<Arc<dyn EventPublisher>>>,
) -> Result<Arc<dyn EventPublisher>> {
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
    client: &dyn EventPublisher,
    old: Option<&PropValue>,
    new: &PropValue,
    seq: u64,
) -> Result<()> {
    let Some(old) = old else {
        return Ok(());
    };
    for (path, old_value, new_value) in cosmix_props_core::diff(old, new) {
        let mut message =
            build_props_changed_message(&path, &old_value, &new_value, "supervisor.state_change");
        message.set("event_seq", &seq.to_string());
        client
            .publish_event(&props_changed_topic(BUS_SERVICE), message)
            .await?;
    }
    Ok(())
}

fn adapter_changed_message(event: &AdapterEvent) -> cosmix_bus::bus::BusMessage {
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", "adapter.changed");
    message.set("event_seq", &event.seq.to_string());
    message.body = json!({
        "event": "adapter.changed",
        "data": {
            "name": event.status.name,
            "service": event.status.service,
            "state": event.status.state.as_str(),
            "previous": event.previous.as_str(),
            "restarts": event.status.restarts,
            "leaked_runs": event.status.leaked_runs,
            "last_error": event.status.last_error,
            "since": rfc3339(event.status.since),
            "event_seq": event.seq,
        }
    })
    .to_string();
    message
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
        assert_eq!(adapters[0]["leaked_runs"], 0);
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

    /// F1 regression: with zero adapters (J1's shipped registry is
    /// empty) the publisher must stay parked on the event stream — not
    /// see it end. Runs the real `supervisor::start` wiring and the real
    /// publisher (no noded: no client ever appears, exactly like a
    /// daemon whose broker is down). On the old code the only events
    /// sender was dropped when `start()` returned, `recv()` yielded
    /// `None`, and the daemon exited into a systemd crash-loop.
    #[tokio::test(start_paused = true)]
    async fn zero_adapter_daemon_keeps_its_event_publisher_alive() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = crate::supervisor::start(
            Vec::new(),
            SessionBus::Unavailable("unset".into()),
            shutdown_rx,
            None,
        );
        let (_client_tx, client_rx) = watch::channel::<Option<Arc<dyn EventPublisher>>>(None);
        let (fault_tx, mut fault_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_publisher(
            started.handle.clone(),
            started.events,
            client_rx,
            fault_tx,
        ));

        // A (virtual) while of zero-adapter uptime: the publisher must
        // still be alive and must not have faulted.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !publisher.is_finished(),
            "zero-adapter daemon must keep its event publisher running"
        );
        assert!(
            fault_rx.try_recv().is_err(),
            "no publisher fault may occur with zero adapters"
        );

        publisher.abort();
    }

    #[test]
    fn adapter_changed_message_carries_the_new_state_and_seq() {
        let event = AdapterEvent {
            status: crate::state::AdapterStatus {
                name: "tray".into(),
                service: "tray".into(),
                state: AdapterStateKind::Backoff,
                restarts: 3,
                leaked_runs: 1,
                last_error: Some("panicked: boom".into()),
                since: SystemTime::UNIX_EPOCH,
            },
            previous: AdapterStateKind::Starting,
            seq: 42,
        };
        let message = adapter_changed_message(&event);
        assert_eq!(message.get("command"), Some("adapter.changed"));
        assert_eq!(message.get("event_seq"), Some("42"));
        let body: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(body["data"]["state"], "backoff");
        assert_eq!(body["data"]["previous"], "starting");
        assert_eq!(body["data"]["restarts"], 3);
        assert_eq!(body["data"]["leaked_runs"], 1);
        assert_eq!(body["data"]["last_error"], "panicked: boom");
        assert_eq!(body["data"]["event_seq"], 42);
    }

    /// The watch reply surfaces the real sequence semantics and the
    /// current counter (F5): a subscriber can tell from `event_seq`
    /// whether it has seen every event.
    #[tokio::test]
    async fn watch_reply_surfaces_the_real_event_sequence() {
        let handle = test_handle().await;
        let (rc, body) = dispatch(
            &command("dbusd.props.watch", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["topic"], "dbusd.props.changed");
        assert_eq!(body["domain_topics"][0], "dbusd.adapter.changed");
        assert!(
            body["event_seq"].is_u64(),
            "watch must carry the current numeric event_seq"
        );
        // Assert the reply's stated semantics, not a keyword any
        // placeholder would contain: it must name the field it
        // describes (event_seq), say what a gap means (dropped
        // events), and point at the bootstrap truth (props.get).
        let semantics = body["event_sequence"].as_str().unwrap();
        assert!(
            semantics.contains("event_seq")
                && semantics.contains("gap")
                && semantics.contains("dbusd.props.get"),
            "event_sequence must state the real semantics: {semantics}"
        );

        let (rc, body) = dispatch(
            &command("dbusd.info", Value::Null),
            &handle,
            &SessionBus::Unavailable("unset".into()),
        );
        assert_eq!(rc, 0);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert!(body["event_seq"].is_u64(), "info carries event_seq");
    }

    /// F8: the props diff baseline survives a publish failure and the
    /// publisher's own reconnect. A subscriber that stayed connected
    /// through the outage must get the accumulated diff on the next
    /// event — on the old code the baseline was reset and the outage
    /// window's changes never reached `dbusd.props.changed` subscribers.
    #[tokio::test(start_paused = true)]
    async fn props_diff_baseline_survives_publisher_reconnect() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FakePublisher {
            fail_next: AtomicUsize,
            published: std::sync::Mutex<Vec<(String, String)>>,
        }

        impl FakePublisher {
            fn fail_next(&self, count: usize) {
                self.fail_next.store(count, Ordering::SeqCst);
            }

            fn published(&self) -> Vec<(String, String)> {
                self.published.lock().expect("fake lock").clone()
            }
        }

        impl EventPublisher for FakePublisher {
            fn publish_event(
                &self,
                topic: &str,
                message: cosmix_bus::bus::BusMessage,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
                let topic = topic.to_string();
                Box::pin(async move {
                    let pending = self.fail_next.load(Ordering::SeqCst);
                    if pending > 0 {
                        self.fail_next.store(pending - 1, Ordering::SeqCst);
                        return Err(anyhow!("scripted publish failure"));
                    }
                    self.published
                        .lock()
                        .expect("fake lock")
                        .push((topic, message.body.clone()));
                    Ok(())
                })
            }
        }

        let factory: crate::adapter::AdapterFactory =
            Arc::new(|| Box::new(crate::fault::FaultAdapter::default()));
        let spec = crate::adapter::AdapterSpec {
            name: "notify".into(),
            service: "notify".into(),
            factory,
        };
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = crate::supervisor::start(
            vec![(spec, true)],
            SessionBus::Unavailable("unset".into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        let mut saw_running = false;
        while !saw_running {
            saw_running = handle
                .status("notify")
                .is_some_and(|status| status.state == AdapterStateKind::Running);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let fake = Arc::new(FakePublisher {
            fail_next: AtomicUsize::new(0),
            published: std::sync::Mutex::new(Vec::new()),
        });
        let (client_tx, client_rx) = watch::channel::<Option<Arc<dyn EventPublisher>>>(None);
        client_tx
            .send(Some(Arc::clone(&fake) as Arc<dyn EventPublisher>))
            .expect("client");
        let (fault_tx, mut fault_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_publisher(
            handle.clone(),
            started.events,
            client_rx,
            fault_tx,
        ));

        // The buffered starting/running events publish; the running
        // snapshot becomes the baseline.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !fake
            .published()
            .iter()
            .any(|(topic, _)| topic == TOPIC_ADAPTER_CHANGED)
        {
            assert!(tokio::time::Instant::now() < deadline, "no initial publish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The outage: the disable event's publish fails, the publisher
        // faults the broker client.
        fake.fail_next(1);
        handle
            .control("notify", LifecycleCmd::Disable)
            .expect("disable");
        fault_rx
            .recv()
            .await
            .expect("publish failure must fault the broker client");

        // Reconnect: the client goes away and comes back, then a state
        // change arrives (enable). The first publish after reconnect
        // must diff against the pre-outage baseline.
        client_tx.send(None).expect("clear client");
        handle
            .control("notify", LifecycleCmd::Enable)
            .expect("enable");
        tokio::time::sleep(Duration::from_millis(10)).await;
        client_tx
            .send(Some(Arc::clone(&fake) as Arc<dyn EventPublisher>))
            .expect("restore client");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let diffs: Vec<(String, String)> = fake
                .published()
                .into_iter()
                .filter(|(topic, _)| topic == "dbusd.props.changed")
                .collect();
            // The enable's changes (state cycle + restart bump; the
            // starting/running states may coalesce, the restart count
            // cannot) must be diffed against the PRE-OUTAGE baseline —
            // restarts was 0 there and 1 after enable. On the old code
            // the baseline was reset and no diff at all reached
            // props.changed after the outage.
            let covered = diffs.iter().any(|(_, body)| {
                let body: Value = serde_json::from_str(body).unwrap();
                body["path"] == "adapters.notify.restarts" && body["old"] == 0 && body["new"] == 1
            });
            if covered {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the accumulated diff (restarts 0 -> 1, against the pre-outage \
                 baseline) never reached props.changed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        publisher.abort();
        shutdown_tx.send(true).expect("shutdown");
        for (name, join) in started.lifecycles {
            tokio::time::timeout(Duration::from_secs(30), join)
                .await
                .unwrap_or_else(|_| panic!("lifecycle '{name}' did not stop"))
                .expect("lifecycle join");
        }
    }
}
