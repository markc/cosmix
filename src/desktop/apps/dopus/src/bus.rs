//! The Bus thread — ced's `bus.rs` shape (itself the
//! `cosmix-term-core/src/bus.rs` shape): a current-thread tokio runtime on its
//! own OS thread holding a [`SupervisedClient`] registered as `dopus` (or
//! `--service NAME`) with `fatal_on_registration_rejection(true)`. It forwards
//! `dopus.*` commands and `theme.changed` topic frames to the app through an
//! unbounded futures channel (exposed as a `Subscription`, no poll thread),
//! reports connection edges, and carries the app's replies back.
//!
//! **No broker is not an error**: [`spawn`] returns [`StartError::Unreachable`]
//! and the app runs windowed without a Bus — a file manager works standalone.
//!
//! P1 security posture (see `verbs.rs`): file-mutating verbs do not exist on
//! the Bus surface; `dopus.action` refuses them in the app layer.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use cosmix_client::{ConnState, IncomingCommand, NodedClient, SupervisedClient};
use iced::futures::channel::mpsc::{UnboundedReceiver, unbounded};

/// Everything the bus thread delivers to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// A `dopus.*` command (topics are separated out below).
    Command(Command),
    /// The `theme.changed` topic fired (body is the theme selection; the app
    /// re-resolves from the files, the same as ctk).
    ThemeChanged,
    Connected,
    Disconnected,
}

/// One request to dopus. `id` indexes a pending reply; `None`-reply verbs
/// still get one (an error reply at least).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub id: u64,
    pub verb: String,
    pub body: String,
    /// `local:<from>` / `mesh:<service>@<peer>` / `anon` (editd E0 §4.3).
    pub caller_key: String,
}

/// Effects the app sends back to the bus thread.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Reply to command `id` with `(rc, body)`.
    Respond { id: u64, rc: u8, body: String },
    /// Stop the bus thread (the app is quitting).
    Quit,
}

/// The handle the app uses to reply / quit.
#[derive(Clone)]
pub struct BusHandle {
    tx: tokio::sync::mpsc::UnboundedSender<Effect>,
}

impl BusHandle {
    pub fn respond(&self, id: u64, rc: u8, body: String) {
        let _ = self.tx.send(Effect::Respond { id, rc, body });
    }

    pub fn quit(&self) {
        let _ = self.tx.send(Effect::Quit);
    }
}

/// Why the Bus could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Another instance owns the service name (single-instance forward).
    NameTaken,
    /// noded refused registration for another reason (message).
    Rejected(String),
    /// No broker reachable — run windowed without a Bus.
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

/// The attested caller key editd would derive (E0 §4.3). noded strips
/// client-supplied `broker_*` headers, so these are the broker's stamps.
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

/// Initial connect + register budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Single-instance probe deadline.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// The topic the shared theme selection announces on.
pub const THEME_TOPIC: &str = "theme.changed";

/// Start the bus thread registered as `service`, connecting to `url`
/// (`cosmix_config::client_helpers::resolve_noded_url()` unless
/// `--noded-url` overrode it).
pub fn spawn(service: &str, url: &str) -> Result<(BusHandle, UnboundedReceiver<Delivery>), StartError> {
    let (dtx, drx) = unbounded();
    let (etx, erx) = tokio::sync::mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let service = service.to_string();
    let url = url.to_owned();
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
    dtx: iced::futures::channel::mpsc::UnboundedSender<Delivery>,
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

    // Commands awaiting a reply from the app.
    let mut commands: HashMap<u64, IncomingCommand> = HashMap::new();
    let mut next_command = 0u64;
    loop {
        tokio::select! {
            cmd = incoming.recv() => {
                let Some(cmd) = cmd else { break };
                if let Some(topic) = cmd.topic() {
                    if topic == THEME_TOPIC {
                        let _ = dtx.unbounded_send(Delivery::ThemeChanged);
                    }
                    continue;
                }
                if cmd.command.is_empty() {
                    continue;
                }
                next_command += 1;
                let delivery = Delivery::Command(Command {
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
                    Effect::Respond { id, rc, body } => {
                        if let Some(cmd) = commands.remove(&id) {
                            let c = client.clone();
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(Duration::from_secs(2), c.respond(&cmd, rc, &body)).await;
                            });
                        }
                    }
                    Effect::Quit => break,
                }
            }
            changed = state.changed() => {
                if changed.is_err() {
                    break;
                }
                let edge = match *state.borrow_and_update() {
                    ConnState::Connected => Some(Delivery::Connected),
                    ConnState::Disconnected => Some(Delivery::Disconnected),
                    ConnState::ShuttingDown | ConnState::Fatal => {
                        break;
                    }
                    ConnState::Connecting => None,
                };
                if let Some(edge) = edge {
                    let _ = dtx.unbounded_send(edge);
                }
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
}

/// One anonymous request to `service`, bounded by `limit`.
fn anonymous_call(url: &str, service: &str, verb: &str, body: &serde_json::Value, limit: Duration) -> Option<(u8, String)> {
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

/// Single-instance probe: an anonymous `dopus.ping` with a 500 ms deadline;
/// `true` when an instance answered.
pub fn probe_running(url: &str, service: &str) -> bool {
    matches!(anonymous_call(url, service, "dopus.ping", &serde_json::json!({}), PROBE_TIMEOUT), Some((0, _)))
}

/// Single-instance forward: send the argv paths as `dopus.open`. P1's running
/// instance accepts and ignores the paths (`verbs::OpenReply.opened` is
/// false), and this still reports success — the forward worked.
pub fn forward_open(url: &str, service: &str, paths: &[String]) -> Result<(), String> {
    match anonymous_call(url, service, "dopus.open", &serde_json::json!({ "paths": paths }), Duration::from_secs(5)) {
        Some((0, _)) => Ok(()),
        Some((rc, body)) => Err(format!("dopus.open refused (rc {rc}): {body}")),
        None => Err(format!("no answer from {service}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(from: &str, headers: &[(&str, &str)]) -> IncomingCommand {
        IncomingCommand {
            from: from.to_string(),
            command: "dopus.ping".into(),
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
