//! The Bus thread (ced E1 plan §2): a current-thread tokio runtime on its own
//! OS thread (the `cosmix-term-core/src/bus.rs:68-107` shape) holding a
//! `SupervisedClient` registered as `ced` (or `--service NAME`) with
//! `fatal_on_registration_rejection(true)`. It correlates requests, arms
//! one-shot timers and deadlines, subscribes topics (`edit.changed`,
//! `theme.changed`, `noded.props.changed`), forwards connection edges, and
//! hands `ced.*` commands to the Controller. Deliveries reach iced through an
//! unbounded futures channel exposed as a `Subscription` (no poll thread).
//!
//! Every wait here is an event: a Bus frame, an effect from the host, a
//! connection-state edge, or a one-shot timer the Controller armed. Requests
//! go out with `call_with_headers_raw` so a refusal's whole `{error_code,
//! message, reason, …}` body reaches the mirror (`call_typed` keeps only the
//! rendered message). A request that fails on the transport (disconnected) is
//! reported as its `Deadline` only once its deadline has passed, so a mirror
//! reconciling while the broker is down retries at most once per deadline —
//! never in a hot loop.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use cosmix_client::{ConnState, IncomingCommand, NodedClient, SupervisedClient};
use cosmix_edit_client::types::Incoming;
use iced::futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::controller::{BusCommand, Effect};

/// Everything the bus thread delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Incoming(Incoming),
    Command(BusCommand),
}

/// Why the Bus could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Another instance owns the service name (single-instance forward).
    NameTaken,
    /// noded refused registration for another reason (message).
    Rejected(String),
    /// No broker reachable.
    Unreachable(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NameTaken => f.write_str("the service name is already registered"),
            StartError::Rejected(m) => write!(f, "registration refused: {m}"),
            StartError::Unreachable(m) => write!(f, "Bus unreachable: {m}"),
        }
    }
}

impl std::error::Error for StartError {}

/// The service every mirror request goes to.
const EDIT: &str = "edit";
/// Initial connect + register budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Single-instance probe deadline (plan §4.8).
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The handle the Controller's host uses to perform [`Effect`]s that need the
/// Bus (sends, replies, timers, subscriptions). Other effects are the host's
/// own (chrome, clipboard, session) and are ignored here.
pub struct BusHandle {
    tx: tokio::sync::mpsc::UnboundedSender<Effect>,
}

impl BusHandle {
    pub fn perform(&self, effect: &Effect) {
        match effect {
            Effect::Send { .. } | Effect::Respond { .. } | Effect::Timer { .. } | Effect::Subscribe { .. } | Effect::Quit => {
                let _ = self.tx.send(effect.clone());
            }
            _ => {}
        }
    }
}

/// The attested caller key editd would derive (E0 §4.3): `local:<from>` for a
/// registered local caller, `mesh:<service>@<peer>` for a mesh caller, else
/// `anon`. noded strips client-supplied `broker_*` headers, so these are the
/// broker's stamps.
pub fn caller_key(cmd: &IncomingCommand) -> String {
    match cmd.header("broker_origin") {
        Some("local") if !cmd.from.is_empty() => format!("local:{}", cmd.from),
        Some("mesh") => format!(
            "mesh:{}@{}",
            cmd.header("broker_service").unwrap_or("unknown"),
            cmd.header("broker_peer").unwrap_or("unknown")
        ),
        _ => "anon".to_string(),
    }
}

/// Start the bus thread registered as `service`.
pub fn spawn(service: &str) -> Result<(BusHandle, UnboundedReceiver<Delivery>), StartError> {
    let (dtx, drx) = unbounded();
    let (etx, erx) = tokio::sync::mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let service = service.to_string();
    let url = cosmix_config::client_helpers::resolve_noded_url();
    std::thread::Builder::new()
        .name(format!("{service}-bus"))
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(StartError::Unreachable(format!("Bus runtime: {e}"))));
                    return;
                }
            };
            runtime.block_on(run(service, url, dtx, erx, ready_tx));
        })
        .map_err(|e| StartError::Unreachable(format!("Bus thread: {e}")))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((BusHandle { tx: etx }, drx)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(StartError::Unreachable("the Bus thread exited".into())),
    }
}

async fn run(
    service: String,
    url: String,
    dtx: UnboundedSender<Delivery>,
    mut erx: tokio::sync::mpsc::UnboundedReceiver<Effect>,
    ready: std::sync::mpsc::Sender<Result<(), StartError>>,
) {
    let connect = SupervisedClient::connect_options(&service, &url).fatal_on_registration_rejection(true).connect();
    let client = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(c)) => Arc::new(c),
        Ok(Err(e)) => {
            let err = match e.registration_rejection() {
                Some((_, msg)) if msg.contains("already registered") => StartError::NameTaken,
                Some((rc, msg)) => StartError::Rejected(format!("rc {rc}: {msg}")),
                None => StartError::Unreachable(e.to_string()),
            };
            let _ = ready.send(Err(err));
            return;
        }
        Err(_) => {
            let _ = ready.send(Err(StartError::Unreachable("connect timed out".into())));
            return;
        }
    };
    let Some(mut incoming) = client.incoming() else {
        let _ = ready.send(Err(StartError::Unreachable("no incoming channel".into())));
        return;
    };
    let mut state = client.subscribe_state();
    let _ = ready.send(Ok(()));

    let mut commands: HashMap<u64, IncomingCommand> = HashMap::new();
    let mut next_command = 0u64;
    loop {
        tokio::select! {
            cmd = incoming.recv() => {
                let Some(cmd) = cmd else { break };
                if let Some(topic) = cmd.topic() {
                    let _ = dtx.unbounded_send(Delivery::Incoming(Incoming::Topic { topic: topic.to_string(), body: cmd.body.clone() }));
                    continue;
                }
                if cmd.command.is_empty() {
                    continue;
                }
                next_command += 1;
                let delivery = Delivery::Command(BusCommand {
                    id: next_command,
                    verb: cmd.command.clone(),
                    body: if cmd.body.trim().is_empty() { "{}".to_string() } else { cmd.body.clone() },
                    caller_key: caller_key(&cmd),
                });
                if cmd.id.is_some() {
                    commands.insert(next_command, cmd);
                }
                let _ = dtx.unbounded_send(delivery);
            }
            effect = erx.recv() => {
                let Some(effect) = effect else { break };
                match effect {
                    Effect::Send { req, out } => {
                        let (c, d) = (client.clone(), dtx.clone());
                        tokio::spawn(async move {
                            let deadline = tokio::time::Instant::now() + Duration::from_millis(out.deadline_ms);
                            let call = c.call_with_headers_raw(EDIT, &out.verb, &BTreeMap::new(), &out.body);
                            let incoming = match tokio::time::timeout_at(deadline, call).await {
                                Ok(Ok((rc, body, _))) => Incoming::Reply { req, rc, body },
                                Ok(Err(_)) => {
                                    tokio::time::sleep_until(deadline).await;
                                    Incoming::Deadline { req }
                                }
                                Err(_) => Incoming::Deadline { req },
                            };
                            let _ = d.unbounded_send(Delivery::Incoming(incoming));
                        });
                    }
                    Effect::Respond { id, rc, body } => {
                        if let Some(cmd) = commands.remove(&id) {
                            let c = client.clone();
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(Duration::from_secs(2), c.respond(&cmd, rc, &body)).await;
                            });
                        }
                    }
                    Effect::Timer { id, ms } => {
                        let d = dtx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            let _ = d.unbounded_send(Delivery::Incoming(Incoming::Timer { id }));
                        });
                    }
                    Effect::Subscribe { topic } => {
                        let c = client.clone();
                        tokio::spawn(async move {
                            if let Err(e) = c.subscribe_topic(&topic).await {
                                tracing::warn!("subscribe {topic}: {e}");
                            }
                        });
                    }
                    Effect::Quit => break,
                    _ => {}
                }
            }
            changed = state.changed() => {
                if changed.is_err() {
                    break;
                }
                let now = *state.borrow_and_update();
                match now {
                    ConnState::Connected => {
                        let _ = dtx.unbounded_send(Delivery::Incoming(Incoming::Connection { up: true }));
                    }
                    ConnState::Disconnected => {
                        let _ = dtx.unbounded_send(Delivery::Incoming(Incoming::Connection { up: false }));
                    }
                    ConnState::ShuttingDown | ConnState::Fatal => break,
                    ConnState::Connecting => {}
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
}

/// One anonymous request to `service`, bounded by `limit`.
fn anonymous_call(service: &str, verb: &str, body: &serde_json::Value, limit: Duration) -> Option<(u8, String)> {
    let url = cosmix_config::client_helpers::resolve_noded_url();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    runtime.block_on(async {
        let call = async {
            let client = NodedClient::connect_anonymous(&url).await.ok()?;
            let reply = client.call_with_headers_raw(service, verb, &BTreeMap::new(), &body.to_string()).await.ok();
            client.close().await;
            reply.map(|(rc, body, _)| (rc, body))
        };
        tokio::time::timeout(limit, call).await.ok().flatten()
    })
}

/// Single-instance probe (plan §4.8): an anonymous `ced.ping` with a 500 ms
/// deadline; `true` when an instance answered. Then `forward_open` sends the
/// argv paths as `ced.open`.
pub fn probe_running(service: &str) -> bool {
    matches!(anonymous_call(service, "ced.ping", &serde_json::json!({}), PROBE_TIMEOUT), Some((0, _)))
}

pub fn forward_open(service: &str, paths: &[String]) -> Result<(), String> {
    match anonymous_call(service, "ced.open", &serde_json::json!({ "paths": paths }), Duration::from_secs(5)) {
        Some((0, _)) => Ok(()),
        Some((rc, body)) => Err(format!("ced.open refused (rc {rc}): {body}")),
        None => Err(format!("no answer from {service}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(from: &str, headers: &[(&str, &str)]) -> IncomingCommand {
        IncomingCommand {
            from: from.to_string(),
            command: "ced.ping".into(),
            id: Some("1".into()),
            args: serde_json::Value::Null,
            body: String::new(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn caller_keys_follow_editd_rules() {
        assert_eq!(caller_key(&cmd("ctl-90", &[("broker_origin", "local")])), "local:ctl-90");
        assert_eq!(caller_key(&cmd("", &[("broker_origin", "local")])), "anon");
        assert_eq!(
            caller_key(&cmd("x", &[("broker_origin", "mesh"), ("broker_service", "svc"), ("broker_peer", "beta")])),
            "mesh:svc@beta"
        );
        assert_eq!(caller_key(&cmd("x", &[])), "anon");
    }
}
