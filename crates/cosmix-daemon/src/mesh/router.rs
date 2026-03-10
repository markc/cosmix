use cosmix_port::amp::{AmpAddress, AmpMessage};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::daemon::state::SharedState;
use super::peers::PeerManager;

/// Route inbound messages from mesh peers.
///
/// Runs as a task consuming the inbound channel. Routes messages to:
/// 1. Pending response map (if it's a response to a request we sent)
/// 2. Local port handlers (if addressed to this node)
/// 3. Port discovery commands (port_list, port_update)
pub async fn run_router(
    node_name: String,
    mut inbound_rx: mpsc::Receiver<(String, AmpMessage)>,
    manager: std::sync::Arc<PeerManager>,
    state: SharedState,
) {
    info!("Mesh router started for node {node_name}");

    while let Some((peer_name, msg)) = inbound_rx.recv().await {
        let msg_type = msg.message_type().unwrap_or("").to_string();
        let command = msg.command_name().unwrap_or("").to_string();

        // 1. Check if this is a response to a pending request
        if msg_type == "response" {
            if let Some(id) = msg.get("id") {
                let id = id.to_string();
                let mut pending = manager.pending.write().await;
                if let Some(tx) = pending.remove(&id) {
                    let _ = tx.send(msg);
                    continue;
                }
            }
        }

        // 2. Handle mesh control commands
        match command.as_str() {
            "hello" => {
                debug!(peer = %peer_name, "hello received");
                // Send port_list in response
                let ports = get_local_port_list(&state);
                let mut reply = AmpMessage::new()
                    .with_header("type", "event")
                    .with_header("from", &format!("{node_name}.amp"))
                    .with_header("command", "port_list");
                reply.body = serde_json::to_string(&ports).unwrap_or_default();
                let _ = manager.send_to(&peer_name, reply).await;
            }

            "port_list" => {
                // Peer is sharing their port list
                if !msg.body.is_empty() {
                    if let Ok(ports) = serde_json::from_str::<Vec<super::peers::RemotePortInfo>>(&msg.body) {
                        info!(peer = %peer_name, count = ports.len(), "received port list");
                        let mut peers = manager.peers.write().await;
                        if let Some(peer_state) = peers.get_mut(&peer_name) {
                            peer_state.remote_ports = ports;
                        }
                    }
                }
            }

            "port_update" => {
                // Peer is notifying about a port change — request fresh list
                let request = AmpMessage::new()
                    .with_header("type", "request")
                    .with_header("from", &format!("{node_name}.amp"))
                    .with_header("command", "port_list");
                let _ = manager.send_to(&peer_name, request).await;
            }

            _ => {
                // 3. Check if addressed to a local port
                if let Some(to) = msg.to_addr() {
                    let to = to.to_string();
                    if let Some(addr) = AmpAddress::parse(&to) {
                        if addr.is_for_node(&node_name) {
                            let response = handle_local_call(&addr, &msg, &state).await;
                            // Send response back to peer
                            let _ = manager.send_to(&peer_name, response).await;
                            continue;
                        }
                    }
                }

                // If it's a request type, route to target
                if msg_type == "request" {
                    if let Err(e) = manager.route(msg).await {
                        warn!(peer = %peer_name, error = %e, "failed to route message");
                    }
                } else {
                    debug!(peer = %peer_name, cmd = %command, "unhandled mesh message");
                }
            }
        }
    }

    info!("Mesh router stopped");
}

/// Handle a port call addressed to this node.
async fn handle_local_call(
    addr: &AmpAddress,
    msg: &AmpMessage,
    state: &SharedState,
) -> AmpMessage {
    let command = msg.command_name().unwrap_or("");
    let args = if msg.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&msg.body).unwrap_or(serde_json::Value::Null)
    };

    // Build response headers
    let mut response = AmpMessage::new()
        .with_header("type", "response");
    if let Some(id) = msg.get("id") {
        response.set("id", id);
    }
    // Swap from/to
    if let Some(from) = msg.from_addr() {
        response.set("to", from);
    }
    if let Some(to) = msg.to_addr() {
        response.set("from", to);
    }

    if let Some(port_name) = &addr.port {
        // Find the port socket
        let socket_path: Option<std::path::PathBuf> = {
            let s = state.read().unwrap();
            s.port_registry.socket_path(port_name)
        };

        match socket_path {
            Some(path) => {
                let path_str: String = path.to_string_lossy().to_string();
                match cosmix_port::call_port(&path_str, command, args).await {
                    Ok(data) => {
                        response.set("rc", "0");
                        response.body = serde_json::to_string(&data).unwrap_or_default();
                    }
                    Err(e) => {
                        response.set("rc", "10");
                        response.set("error", &e.to_string());
                    }
                }
            }
            None => {
                response.set("rc", "10");
                response.set("error", &format!("port '{}' not found on this node", port_name));
            }
        }
    } else {
        response.set("rc", "10");
        response.set("error", "no port specified in address");
    }

    response
}

/// Get list of local ports for sharing with peers.
fn get_local_port_list(state: &SharedState) -> Vec<super::peers::RemotePortInfo> {
    let s = state.read().unwrap();
    let mut ports = Vec::new();

    // Built-in ports
    for name in &["clipboard", "windows", "screenshot", "dbus", "config", "mail", "midi", "notify", "input"] {
        ports.push(super::peers::RemotePortInfo {
            name: name.to_string(),
            commands: Vec::new(), // built-ins don't enumerate commands here
        });
    }

    // App ports from registry
    for info in s.port_registry.ports.values() {
        ports.push(super::peers::RemotePortInfo {
            name: info.name.clone(),
            commands: info.commands.clone(),
        });
    }

    ports
}
