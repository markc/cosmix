//! Standalone-host client of comp's panel holder plane (shell design §9).
//!
//! Quoin reports each panel's actual mode and the corner menu's popup hold per
//! `(output, edge)`, naming the layer by its unique namespace token, and
//! receives comp's reveal/conceal commands. Everything is gated on comp's
//! `input.corners.holders` leaf: until a read of it answers `true` on the
//! current connection nothing is sent and every command is dropped, so a comp
//! without the plane leaves today's local behaviour untouched. The host hands
//! the gate to the model (`ShellCommandKind::HolderPlane`), which is
//! command-driven exactly while it is open; any doubt closes it.
//!
//! Comp verbs are literal `comp.*` commands addressed to the selected service;
//! only the subscribed topics carry the service name.
//!
//! Event-driven: reads and replays follow connection, registry, property,
//! mapping and gap observations. The one timer is a one-shot retry after a
//! transient failure, whose delay doubles up to [`RETRY_CAP`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::Real;
use cosmix_shell::core::{Edge, OutputKey, PanelEffect, PanelInput, PanelMode};
use cosmix_shell::runtime::{ShellCommand, ShellCommandKind, ShellEffects, ShellFrameState};
use cosmix_shell_host::LayerHostDeadline;
use cosmix_shell_host::holders::{PanelLayerIdentities, PopupLayerIdentity};
use ctk::bus::{BusBridge, BusBridgeConfig, BusBridgeEvent, BusConnectionState, BusMessage, BusReply};
use serde_json::{Value, json};

use crate::bus_service::edge_name;
use crate::hotspot::{COMP_PROPS_GET, is_comp_gap};

const HOLDERS_PATH: &str = "input.corners.holders";
const TOPIC_SUFFIXES: [&str; 3] = ["props.changed", "panel.command", "surface.mapped"];
const RETRY_FIRST: Duration = Duration::from_millis(250);
const RETRY_CAP: Duration = Duration::from_secs(8);

/// `(layer token, verb)`: one desired request per layer and verb.
type Key = (String, String);

/// One accepted comp command, already matched to a current layer token. It
/// drives the model only while the model is command-driven, which the host
/// keeps in step with [`HolderClient::capable`] through
/// [`HolderClient::plane_change`]; the model ignores it otherwise.
#[derive(Debug, PartialEq)]
pub(crate) struct HolderCommand {
    pub(crate) output: String,
    pub(crate) edge: Edge,
    pub(crate) reveal: bool,
}

impl HolderCommand {
    /// The model input on the named output: comp's reveal is a holder gained,
    /// its conceal the last holder released (its delay already served).
    pub(crate) fn shell_command(&self, at: Duration) -> Option<ShellCommand> {
        Some(ShellCommand {
            output: OutputKey::new(self.output.clone()).ok()?,
            at,
            kind: ShellCommandKind::Panel {
                edge: self.edge,
                input: if self.reveal { PanelInput::HolderReveal } else { PanelInput::HolderConceal },
            },
        })
    }
}

/// What a refused request waits for. An unchanged request is resent only
/// when its trigger arrives; a changed one is always sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    /// The layer or its output is not there yet: the next `surface.mapped`.
    Mapping,
    /// Refused under a session lock: the next `focus.session_lock` change.
    Unlock,
    /// Busy comp, timeout or transport failure: the one-shot retry deadline.
    Retry,
    /// Malformed or unsupported: only a changed intent.
    Intent,
}

#[derive(Resource)]
pub(crate) struct HolderClient {
    service: String,
    generation: Option<u64>,
    /// The latest registry verdict on comp; `None` until one arrives on the
    /// current connection.
    present: Option<bool>,
    pub(crate) capable: bool,
    /// The capability last handed to the model; see [`HolderClient::plane_change`].
    plane_reported: bool,
    read_needed: bool,
    capability_read: Option<u64>,
    next_id: u64,
    last_sequence: u64,
    // Desired state survives a Bus reconnect and is replayed after capability.
    desired: BTreeMap<Key, Value>,
    acknowledged: BTreeMap<Key, Value>,
    failed: BTreeMap<Key, (Value, Wait)>,
    /// Acquisitions comp may still record: sent, and no release acknowledged
    /// since. Comp keeps holds while Quoin reconnects or while its own
    /// registration lapses, so this survives invalidation and presence
    /// changes, and a menu that closed meanwhile is still released. Only an
    /// acknowledged release, or one refused `unknown_output`, retires it.
    maybe_held: BTreeMap<Key, Value>,
    pending: Option<(u64, Key, Value)>,
    /// A mode change happened while the pending report was in flight: its
    /// acknowledgement must not stand for the change comp has not seen.
    pending_superseded: bool,
    retry_wanted: bool,
    retry_read: bool,
    retry_at: Option<Duration>,
    backoff: Duration,
    popup: BTreeMap<(String, String), bool>,
    popup_surfaces: BTreeMap<(String, String), String>,
}

pub(crate) fn install(app: &mut App, bus: &mut BusBridgeConfig, service: String) {
    for suffix in TOPIC_SUFFIXES {
        let topic = format!("{service}.{suffix}");
        if !bus.subscriptions.contains(&topic) {
            bus.subscriptions.push(topic);
        }
    }
    app.insert_resource(HolderClient::new(service));
}

/// `Ok` for an accepted reply, else the refusal code (`None` when the call
/// itself failed or the body names none).
fn refusal(result: &Result<BusReply, String>) -> Result<(), Option<String>> {
    match result {
        Ok(reply) if reply.rc == 0 => Ok(()),
        Ok(reply) => Err(serde_json::from_str::<Value>(&reply.body)
            .ok()
            .and_then(|body| body.get("error")?.as_str().map(str::to_owned))),
        Err(_) => Err(None),
    }
}

fn wait_for(code: Option<&str>) -> Wait {
    match code {
        Some(
            "unknown_panel_surface" | "unknown_output" | "panel_output_mismatch"
            | "ambiguous_panel_surface",
        ) => Wait::Mapping,
        Some("locked") => Wait::Unlock,
        None | Some("busy") => Wait::Retry,
        Some(_) => Wait::Intent,
    }
}

impl HolderClient {
    fn new(service: String) -> Self {
        Self {
            service, generation: None, present: None, capable: false, plane_reported: false,
            read_needed: false,
            capability_read: None, next_id: 0x49_0000_0000, last_sequence: 0,
            desired: BTreeMap::new(), acknowledged: BTreeMap::new(), failed: BTreeMap::new(),
            maybe_held: BTreeMap::new(), pending: None, pending_superseded: false,
            retry_wanted: false, retry_read: false,
            retry_at: None, backoff: RETRY_FIRST, popup: BTreeMap::new(),
            popup_surfaces: BTreeMap::new(),
        }
    }

    /// Doubt about comp's state: close the gate, forget what comp has seen,
    /// and (when connected) read the capability again before replaying.
    fn invalidate(&mut self, reread: bool) {
        self.capable = false;
        self.capability_read = None;
        self.read_needed = reread && self.generation.is_some();
        self.pending = None;
        self.pending_superseded = false;
        self.acknowledged.clear();
        self.failed.clear();
        self.last_sequence = 0;
        self.retry_wanted = false;
        self.retry_read = false;
        self.retry_at = None;
        self.backoff = RETRY_FIRST;
    }

    /// The capability to hand the model when it differs from what the model
    /// was last told. The host calls this after draining events and again
    /// after messages, so a gate that opened is applied before the commands it
    /// admits and one that closed returns the model to local rules at once.
    pub(crate) fn plane_change(&mut self) -> Option<bool> {
        (self.plane_reported != self.capable).then(|| {
            self.plane_reported = self.capable;
            self.capable
        })
    }

    /// The model changed an edge's mode. The command-driven model waits for
    /// comp's verdict on the hidden report that follows (an unpin keeps its
    /// reveal until then, a hide latches until then), so that report must be
    /// sent and answered even when its body equals the last one acknowledged:
    /// a pin and unpin in one pass, or both inside a retry backoff.
    pub(crate) fn mode_changed(&mut self, output: &str, edge: &str) {
        let stale = |key: &Key, body: &Value| {
            key.1 == "panel.mode" && body["output"] == output && body["edge"] == edge
        };
        self.acknowledged.retain(|key, body| !stale(key, body));
        self.failed.retain(|key, (body, _)| !stale(key, body));
        if self.pending.as_ref().is_some_and(|(_, key, body)| stale(key, body)) {
            self.pending_superseded = true;
        }
    }

    pub(crate) fn presence(&mut self, live: &BTreeSet<String>) {
        let present = live.contains(&self.service);
        match (self.present.replace(present), present) {
            // The first verdict on this connection: its read is under way.
            (None, true) | (Some(false), false) => {}
            // A receipt without a transition, usually unrelated registry
            // churn. Comp may also have re-registered between observations
            // and lost its state, so re-read and replay (both idempotent),
            // but keep the gate open: no command is dropped meanwhile.
            (Some(true), true) => {
                self.read_needed |= self.generation.is_some() && self.capability_read.is_none();
                self.acknowledged.clear();
                self.failed.clear();
                // A restarted comp counts event_seq from zero again.
                self.last_sequence = 0;
            }
            // Comp left or arrived. `maybe_held` stays: a living comp can lose
            // its registration and keep its holds, and a release to a comp
            // that has none is a harmless no-op.
            _ => self.invalidate(present),
        }
    }

    pub(crate) fn event(&mut self, event: &BusBridgeEvent) {
        match event {
            BusBridgeEvent::Connection { state: BusConnectionState::Connected, generation } => {
                self.generation = Some(*generation);
                self.present = None;
                self.invalidate(true);
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                self.generation = None;
                self.present = None;
                self.invalidate(false);
            }
            // The inbound queue overflowed: commands may be among the losses.
            BusBridgeEvent::DroppedMessages(_) => self.invalidate(true),
            BusBridgeEvent::Reply { request_id, result } if self.capability_read == Some(*request_id) => {
                self.capability_read = None;
                match (refusal(result), result) {
                    (Ok(()), Ok(reply)) => {
                        self.backoff = RETRY_FIRST;
                        self.capable = serde_json::from_str::<Value>(&reply.body).ok() == Some(json!(true));
                    }
                    // A background re-read keeps its answer until a real one.
                    (Err(code), _) if wait_for(code.as_deref()) == Wait::Retry => {
                        self.retry_read = true;
                        self.retry_wanted = true;
                    }
                    _ => self.capable = false,
                }
            }
            BusBridgeEvent::Reply { request_id, result }
                if self.pending.as_ref().is_some_and(|(id, _, _)| id == request_id) =>
            {
                let (_, key, body) = self.pending.take().unwrap();
                let superseded = std::mem::take(&mut self.pending_superseded);
                self.settle(key.clone(), body, result);
                if superseded {
                    self.acknowledged.remove(&key);
                }
            }
            _ => {}
        }
    }

    fn settle(&mut self, key: Key, body: Value, result: &Result<BusReply, String>) {
        let release = body["acquire"] == false;
        match refusal(result) {
            Ok(()) => {
                self.backoff = RETRY_FIRST;
                if key.1 == "panel.mode" && body["mode"] != "hidden" {
                    // Persistent modes clear comp's holds on that edge, so a
                    // later hidden mode must acquire even an unchanged intent.
                    let same_edge = |v: &Value| v["output"] == body["output"] && v["edge"] == body["edge"];
                    self.acknowledged.retain(|k, v| k.1 != "panel.hold" || !same_edge(v));
                    self.maybe_held.retain(|_, v| !same_edge(v));
                }
                if release {
                    self.maybe_held.remove(&key);
                }
                self.acknowledged.insert(key, body);
            }
            // Comp dropped a removed output's holds: nothing is left to release.
            Err(Some(code)) if release && code == "unknown_output" => {
                self.maybe_held.remove(&key);
                self.acknowledged.insert(key, body);
            }
            Err(code) => {
                let wait = wait_for(code.as_deref());
                self.retry_wanted |= wait == Wait::Retry;
                self.failed.insert(key, (body, wait));
            }
        }
    }

    pub(crate) fn message(&mut self, message: &BusMessage) -> Option<HolderCommand> {
        if self.generation != Some(message.connection_generation) {
            return None;
        }
        let suffix = message.topic()?.strip_prefix(self.service.as_str())?.strip_prefix('.')?;
        if !TOPIC_SUFFIXES.contains(&suffix) {
            return None;
        }
        let body: Value = serde_json::from_str(&message.body).ok()?;
        if is_comp_gap(&body) {
            // Lost records may include commands: fall back to local behaviour
            // until a fresh read, then replay everything.
            self.invalidate(true);
            return None;
        }
        match suffix {
            "surface.mapped" => {
                self.failed.retain(|_, (_, wait)| *wait != Wait::Mapping);
                return None;
            }
            "props.changed" => {
                match body["path"].as_str() {
                    Some(HOLDERS_PATH | "input.corners" | "input") => self.invalidate(true),
                    Some("focus.session_lock" | "focus") => {
                        self.failed.retain(|_, (_, wait)| *wait != Wait::Unlock);
                    }
                    _ => {}
                }
                return None;
            }
            _ => {}
        }
        if !self.capable || body["version"] != 1 {
            return None;
        }
        let sequence = body["event_seq"].as_u64()?;
        if sequence <= self.last_sequence {
            return None;
        }
        let edge = match body["edge"].as_str()? {
            "top" => Edge::Top, "bottom" => Edge::Bottom,
            "left" => Edge::Left, "right" => Edge::Right, _ => return None,
        };
        let reveal = match body["action"].as_str()? {
            "reveal" => true, "conceal" => false, _ => return None,
        };
        let output = body["output"].as_str()?.to_owned();
        let surface = body["surface"].as_str()?.to_owned();
        // An old layer's delayed command must not control its replacement.
        let current_panel = self.desired.iter().any(|(key, value)| key.1 == "panel.mode"
            && value["surface"] == surface && value["output"] == output && value["edge"] == body["edge"]);
        let current_popup = self.popup_surfaces.get(&(output.clone(), edge_name(edge).into())) == Some(&surface);
        if !current_panel && !current_popup {
            return None;
        }
        self.last_sequence = sequence;
        Some(HolderCommand { output, edge, reveal })
    }

    /// One host update: fire a due retry, send, then arm a deadline for any
    /// send that just failed, so the next unrelated update cannot resend early.
    fn update(&mut self, bridge: &BusBridge, now: Duration, deadline: &mut LayerHostDeadline) {
        self.tick(now, deadline);
        self.flush(bridge);
        self.tick(now, deadline);
    }

    /// Fires the one-shot retry, then arms the next one if a transient failure
    /// asked for it, and publishes it as the host's wake deadline. The cap
    /// bounds the delay, not the attempts: a comp that stays busy is retried
    /// every [`RETRY_CAP`] until it answers or the connection changes. Capping
    /// attempts instead would give up on a release and strand comp's hold;
    /// comp leaving or a success are the only exits.
    fn tick(&mut self, now: Duration, deadline: &mut LayerHostDeadline) {
        if self.retry_at.is_some_and(|at| at <= now) {
            // One firing covers every transient failure so far.
            self.retry_at = None;
            self.retry_wanted = false;
            self.failed.retain(|_, (_, wait)| *wait != Wait::Retry);
            if std::mem::take(&mut self.retry_read) && self.generation.is_some() {
                self.read_needed = true;
            }
        }
        if self.retry_wanted && self.retry_at.is_none() {
            self.retry_at = Some(now + self.backoff);
            self.backoff = (self.backoff * 2).min(RETRY_CAP);
        }
        if let Some(at) = self.retry_at {
            deadline.0 = Some(deadline.0.map_or(at, |current| current.min(at)));
        }
    }

    fn mode_acknowledged(&self, hold: &Value) -> bool {
        self.desired.iter().any(|(key, mode)| key.1 == "panel.mode"
            && mode["output"] == hold["output"] && mode["edge"] == hold["edge"]
            && self.acknowledged.get(key) == Some(mode))
    }

    /// Sends nothing while a retry is armed: a full outbound queue or a busy
    /// comp backs off everything, not just the request that met it.
    fn flush(&mut self, bridge: &BusBridge) {
        if self.generation.is_none() || self.retry_at.is_some() {
            return;
        }
        if self.read_needed && self.capability_read.is_none() {
            self.next_id += 1;
            if bridge.try_call(self.next_id, &self.service, COMP_PROPS_GET, BTreeMap::new(),
                json!({"path": HOLDERS_PATH}).to_string()).is_ok() {
                self.capability_read = Some(self.next_id);
                self.read_needed = false;
            } else {
                self.retry_wanted = true;
                return;
            }
        }
        if !self.capable || self.pending.is_some() {
            return;
        }
        // Mode reports precede acquisitions, including replay on reconnect.
        // A release needs no mode first: its edge may no longer be reported.
        let next = ["panel.mode", "panel.hold"].into_iter().find_map(|verb| {
            self.desired.iter().find(|(key, body)| key.1 == verb
                && self.acknowledged.get(*key) != Some(*body)
                && self.failed.get(*key).is_none_or(|(failed, _)| failed != *body)
                && (body["acquire"] != true || self.mode_acknowledged(body)))
                .map(|(key, body)| (key.clone(), body.clone()))
        });
        if let Some((key, body)) = next {
            self.next_id += 1;
            if bridge.try_call(self.next_id, &self.service, format!("comp.{}", key.1),
                BTreeMap::new(), body.to_string()).is_ok() {
                if body["acquire"] == true {
                    self.maybe_held.insert(key.clone(), body.clone());
                }
                self.pending = Some((self.next_id, key, body));
            } else {
                self.retry_wanted = true;
            }
        }
    }
}

/// Mirror the actual mode after Model, and the existing chunk-11 menu holds.
/// Every hidden mode report comp accepts draws its current reveal/conceal
/// verdict, which is how a model that just went command-driven learns it.
pub(crate) fn report_holders(
    bridge: Res<BusBridge>,
    (time, mut deadline): (Res<Time<Real>>, ResMut<LayerHostDeadline>),
    mut client: Option<ResMut<HolderClient>>,
    identities: Option<Res<PanelLayerIdentities>>,
    popup_identity: Option<Res<PopupLayerIdentity>>,
    (frame, effects): (Res<ShellFrameState>, Option<Res<ShellEffects>>),
    mut commands: MessageReader<ShellCommand>,
) {
    let Some(client) = client.as_deref_mut() else { commands.clear(); return; };
    let output = &frame.0.geometry.output;
    for effect in effects.iter().flat_map(|effects| &effects.0) {
        if matches!(effect.effect, PanelEffect::ModeChanged { .. }) {
            client.mode_changed(output.as_str(), edge_name(effect.edge));
        }
    }
    for command in commands.read() {
        if let ShellCommandKind::Panel { edge, input: cosmix_shell::core::PanelInput::MenuHold(open) } = &command.kind {
            client.popup.insert((command.output.as_str().into(), edge_name(*edge).into()), *open);
        }
    }
    if let Some(identity) = &popup_identity {
        client.popup_surfaces.insert((identity.output.as_str().into(), edge_name(identity.edge).into()),
            identity.surface.clone());
    }
    client.desired.clear();
    if let Some(identities) = identities {
        for edge in Edge::ALL {
            let Some(surface) = identities.get(output, edge) else { continue; };
            let mode = frame.0.panel(edge).mode;
            client.desired.insert((surface.into(), "panel.mode".into()), json!({
                "output":output.as_str(),"edge":edge_name(edge),"surface":surface,"mode":mode.as_str(),
            }));
            let popup_key = (output.as_str().into(), edge_name(edge).into());
            let Some(popup_surface) = client.popup_surfaces.get(&popup_key).cloned() else { continue; };
            let acquire = mode == PanelMode::Hidden
                && client.popup.get(&popup_key).copied().unwrap_or(false)
                && popup_identity.as_ref().is_some_and(|p| p.output == *output && p.edge == edge);
            let key = (popup_surface.clone(), "panel.hold".to_owned());
            // A closed menu's token goes quiet once its release is acknowledged.
            if acquire || client.maybe_held.contains_key(&key) {
                client.desired.insert(key, json!({
                    "output":output.as_str(),"edge":edge_name(edge),"surface":popup_surface,
                    "holder":"popup","acquire":acquire,
                }));
            }
        }
    }
    // Every hold comp may still record is released unless it is wanted now:
    // a replaced menu's, and one left on an output Quoin no longer shows.
    let releases: Vec<_> = client.maybe_held.iter()
        .filter(|(key, _)| !client.desired.contains_key(*key))
        .map(|(key, body)| {
            let mut release = body.clone();
            release["acquire"] = json!(false);
            (key.clone(), release)
        })
        .collect();
    client.desired.extend(releases);
    let live: BTreeSet<_> = client.desired.keys().cloned().collect();
    client.acknowledged.retain(|key, _| live.contains(key));
    client.failed.retain(|key, _| live.contains(key));
    client.popup.retain(|(popup_output, _), _| popup_output == output.as_str());
    client.popup_surfaces.retain(|(popup_output, _), _| popup_output == output.as_str());
    client.update(&bridge, time.elapsed(), &mut deadline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::bus::{TestBusPeer, test_bridge};

    fn connected(generation: u64) -> BusBridgeEvent {
        BusBridgeEvent::Connection { state: BusConnectionState::Connected, generation }
    }

    fn reply(request_id: u64, rc: u8, body: &str) -> BusBridgeEvent {
        BusBridgeEvent::Reply {
            request_id,
            result: Ok(BusReply { rc, body: body.into(), result: None }),
        }
    }

    fn comp() -> BTreeSet<String> {
        BTreeSet::from(["comp-nested".to_owned()])
    }

    /// A comp observation frame as comp publishes it: topic, command and
    /// sequence headers, everything else in the body.
    fn frame(suffix: &str, body: Value) -> BusMessage {
        BusMessage {
            connection_generation: 1,
            from: "comp-nested".into(),
            command: suffix.into(),
            body: body.to_string(),
            headers: BTreeMap::from([
                ("topic".into(), format!("comp-nested.{suffix}")),
                ("command".into(), suffix.into()),
                ("event_seq".into(), "7".into()),
            ]),
        }
    }

    fn command(surface: &str, event_seq: u64) -> BusMessage {
        frame("panel.command", json!({"version":1,"output":"test","edge":"left",
            "surface":surface,"action":"reveal","event_seq":event_seq}))
    }

    fn mode_key(token: &str) -> Key {
        (token.into(), "panel.mode".into())
    }

    fn mode_body(token: &str) -> Value {
        json!({"output":"test","edge":"left","surface":token,"mode":"hidden"})
    }

    /// Connected, comp registered, and the leaf answered `true`.
    fn capable_client() -> (HolderClient, BusBridge, TestBusPeer) {
        let (bridge, peer) = test_bridge("shell");
        let mut client = HolderClient::new("comp-nested".into());
        client.event(&connected(1));
        client.presence(&comp());
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1, "exactly one read for connect + first verdict");
        assert_eq!((calls[0].to.as_str(), calls[0].command.as_str()), ("comp-nested", "comp.props.get"));
        assert_eq!(calls[0].body, r#"{"path":"input.corners.holders"}"#);
        client.event(&reply(calls[0].request_id, 0, "true"));
        assert!(client.capable);
        (client, bridge, peer)
    }

    /// Sends the desired mode report and acknowledges it.
    fn acknowledge_mode(client: &mut HolderClient, bridge: &BusBridge, peer: &TestBusPeer, token: &str) {
        client.desired.insert(mode_key(token), mode_body(token));
        client.flush(bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.panel.mode");
        client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
    }

    #[test]
    fn commands_ignored_while_uncapable() {
        let mut client = HolderClient::new("comp-nested".into());
        client.event(&connected(1));
        client.desired.insert(mode_key("token"), mode_body("token"));
        let (bridge, peer) = test_bridge("shell");
        client.flush(&bridge);
        let id = peer.drain_calls()[0].request_id;
        assert!(client.message(&command("token", 1)).is_none());
        client.event(&reply(id, 0, "false"));
        assert!(client.message(&command("token", 1)).is_none());
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "a false leaf sends nothing");
        // An old comp without the leaf refuses the path: also inert.
        client.invalidate(true);
        client.flush(&bridge);
        let id = peer.drain_calls()[0].request_id;
        client.event(&reply(id, 10, r#"{"error":"unknown_path"}"#));
        assert!(!client.capable);
        client.invalidate(true);
        client.flush(&bridge);
        let id = peer.drain_calls()[0].request_id;
        client.event(&reply(id, 0, "true"));
        assert!(client.message(&command("token", 1)).unwrap().reveal);
        assert!(client.message(&command("token", 1)).is_none(), "duplicate sequence");
        client.event(&BusBridgeEvent::DroppedMessages(1));
        assert!(!client.capable);
        assert!(client.message(&command("token", 2)).is_none());
        client.event(&connected(2));
        client.event(&reply(id, 0, "true"));
        assert!(!client.capable, "old lifetime's reply cannot enable the gate");
        assert!(client.message(&command("token", 2)).is_none());
    }

    #[test]
    fn stale_token_commands_are_dropped() {
        let (mut client, bridge, peer) = capable_client();
        acknowledge_mode(&mut client, &bridge, &peer, "panel-2");
        assert!(client.message(&command("panel-1", 5)).is_none(), "replaced layer's command");
        assert!(client.message(&command("panel-2", 5)).is_some());
        assert!(client.message(&command("panel-2", 4)).is_none(), "older sequence");
        let mut elsewhere = command("panel-2", 6);
        elsewhere.headers.insert("topic".into(), "comp.panel.command".into());
        assert!(client.message(&elsewhere).is_none(), "another comp's topic");
    }

    /// Comp's gap frame, byte for byte: the marker is in the body only.
    #[test]
    fn comp_gap_frame_invalidates_and_replays() {
        let (mut client, bridge, peer) = capable_client();
        acknowledge_mode(&mut client, &bridge, &peer, "panel-1");
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        let gap = frame("panel.command", json!({"gap":true,"lost_count":3,"cause":"outbox.overflow"}));
        assert!(!gap.headers.contains_key("gap"));
        assert!(client.message(&gap).is_none());
        assert!(!client.capable, "lost commands: back to local behaviour");
        assert!(client.message(&command("panel-1", 9)).is_none());
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.props.get");
        client.event(&reply(calls[0].request_id, 0, "true"));
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.panel.mode", "desired state is replayed");
    }

    #[test]
    fn registry_receipt_keeps_the_gate_open_and_replays() {
        let (mut client, bridge, peer) = capable_client();
        acknowledge_mode(&mut client, &bridge, &peer, "panel-1");
        client.presence(&comp());
        assert!(client.capable, "unrelated registry churn keeps the gate");
        assert!(client.message(&command("panel-1", 3)).is_some());
        client.flush(&bridge);
        let commands: Vec<_> = peer.drain_calls().into_iter().map(|call| call.command).collect();
        assert_eq!(commands, ["comp.props.get", "comp.panel.mode"], "background re-read + replay");
        assert!(client.message(&command("panel-1", 1)).is_none(), "an old sequence");
        // A comp restarted between observations counts event_seq from zero.
        client.presence(&comp());
        assert!(client.message(&command("panel-1", 1)).is_some(), "a lower sequence after a receipt");
        client.presence(&BTreeSet::new());
        assert!(!client.capable);
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "absent comp: nothing to read");
        client.presence(&comp());
        client.flush(&bridge);
        assert_eq!(peer.drain_calls()[0].command, "comp.props.get");
    }

    #[test]
    fn transient_refusal_retries_on_a_one_shot_deadline() {
        let (mut client, bridge, peer) = capable_client();
        let mut deadline = LayerHostDeadline::default();
        client.tick(Duration::ZERO, &mut deadline);
        assert_eq!(deadline.0, None, "nothing failed: no timer");
        client.desired.insert(mode_key("panel-1"), mode_body("panel-1"));
        client.flush(&bridge);
        client.event(&reply(peer.drain_calls()[0].request_id, 10, r#"{"error":"busy"}"#));
        client.tick(Duration::ZERO, &mut deadline);
        assert_eq!(deadline.0, Some(RETRY_FIRST));
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "no resend before the deadline");
        client.tick(RETRY_FIRST, &mut deadline);
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1, "the deadline resends once");
        // A timeout (transport error) doubles the delay.
        client.event(&BusBridgeEvent::Reply {
            request_id: calls[0].request_id,
            result: Err("Bus request timed out after 2 s".into()),
        });
        deadline.0 = None;
        client.tick(RETRY_FIRST, &mut deadline);
        assert_eq!(deadline.0, Some(RETRY_FIRST + RETRY_FIRST * 2));
        client.tick(RETRY_FIRST * 3, &mut deadline);
        client.flush(&bridge);
        let calls = peer.drain_calls();
        client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
        deadline.0 = None;
        client.tick(RETRY_FIRST * 4, &mut deadline);
        assert_eq!(deadline.0, None, "success arms nothing");
        assert_eq!(client.backoff, RETRY_FIRST);
    }

    /// A full outbound queue is a transient failure too: it arms the deadline
    /// in the same update, and nothing is sent until that deadline fires.
    #[test]
    fn immediate_send_failure_backs_off_until_the_deadline() {
        let (mut client, bridge, peer) = capable_client();
        let mut deadline = LayerHostDeadline::default();
        let fill = || while bridge.try_call(0, "filler", "filler", BTreeMap::new(), "").is_ok() {};
        let sent = || peer.drain_calls().into_iter().filter(|call| call.to == "comp-nested").count();
        client.desired.insert(mode_key("panel-1"), mode_body("panel-1"));
        fill();
        client.update(&bridge, Duration::ZERO, &mut deadline);
        assert_eq!(deadline.0, Some(RETRY_FIRST), "the failed send armed a deadline");
        assert_eq!(sent(), 0);
        client.update(&bridge, RETRY_FIRST / 2, &mut deadline);
        assert_eq!(sent(), 0, "an unrelated update before the deadline sends nothing");
        fill();
        client.update(&bridge, RETRY_FIRST, &mut deadline);
        assert_eq!(sent(), 0, "the expiry's one attempt failed again");
        assert_eq!(client.retry_at, Some(RETRY_FIRST * 3), "and doubled the delay");
        client.update(&bridge, RETRY_FIRST * 2, &mut deadline);
        assert_eq!(sent(), 0);
        client.update(&bridge, RETRY_FIRST * 3, &mut deadline);
        let calls: Vec<_> = peer.drain_calls();
        assert_eq!(calls.len(), 1, "exactly one send at the expiry");
        assert_eq!(calls[0].command, "comp.panel.mode");
        client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
        assert_eq!(client.backoff, RETRY_FIRST);
        client.update(&bridge, RETRY_FIRST * 4, &mut deadline);
        assert_eq!(client.retry_at, None, "no re-arm without a new failure");
    }

    #[test]
    fn refused_hold_waits_for_its_layer_or_unlock() {
        let (mut client, bridge, peer) = capable_client();
        let mut deadline = LayerHostDeadline::default();
        acknowledge_mode(&mut client, &bridge, &peer, "panel-1");
        let hold = json!({"output":"test","edge":"left","surface":"menu-1","holder":"popup","acquire":true});
        client.desired.insert(("menu-1".into(), "panel.hold".into()), hold);
        for (code, wakes, others) in [
            ("unknown_panel_surface", frame("surface.mapped", json!({"id":"s9"})),
                frame("props.changed", json!({"path":"focus.session_lock","new":"none"}))),
            ("locked", frame("props.changed", json!({"path":"focus.session_lock","new":"none"})),
                frame("surface.mapped", json!({"id":"s9"}))),
        ] {
            client.flush(&bridge);
            let calls = peer.drain_calls();
            assert_eq!(calls[0].command, "comp.panel.hold");
            client.event(&reply(calls[0].request_id, 10, &json!({"error":code}).to_string()));
            client.tick(Duration::ZERO, &mut deadline);
            assert_eq!(deadline.0, None, "{code}: a refusal is not retried on a timer");
            client.message(&others);
            client.flush(&bridge);
            assert!(peer.drain_calls().is_empty(), "{code}: the wrong trigger");
            client.message(&wakes);
            client.flush(&bridge);
            let calls = peer.drain_calls();
            assert_eq!(calls.len(), 1, "{code}: its trigger resends");
            client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
            client.acknowledged.clear();
            client.desired.insert(mode_key("panel-1"), mode_body("panel-1"));
            client.acknowledged.insert(mode_key("panel-1"), mode_body("panel-1"));
        }
    }

    /// A pin and unpin in one pass nets to the report comp already holds,
    /// yet the model waits for comp's verdict on it: it is sent again.
    #[test]
    fn mode_change_resends_an_unchanged_report() {
        let (mut client, bridge, peer) = capable_client();
        acknowledge_mode(&mut client, &bridge, &peer, "panel-1");
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "an acknowledged report is quiet");
        client.mode_changed("test", "left");
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.panel.mode");
        client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "answered once, quiet again");
        // Another edge's change leaves this report alone.
        client.mode_changed("test", "top");
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
    }

    /// The same when the change lands while the old report is in flight or
    /// while a retry backoff holds every send.
    #[test]
    fn mode_change_survives_in_flight_reports_and_backoff() {
        let (mut client, bridge, peer) = capable_client();
        client.desired.insert(mode_key("panel-1"), mode_body("panel-1"));
        client.flush(&bridge);
        let in_flight = peer.drain_calls();
        client.mode_changed("test", "left");
        client.event(&reply(in_flight[0].request_id, 0, r#"{"accepted":true}"#));
        client.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1, "the superseded acknowledgement does not stand");
        client.event(&reply(calls[0].request_id, 0, r#"{"accepted":true}"#));
        // Backoff: another request met a busy comp and armed the deadline.
        let mut deadline = LayerHostDeadline::default();
        client.desired.insert(mode_key("panel-2"), json!({"output":"test","edge":"top",
            "surface":"panel-2","mode":"hidden"}));
        client.flush(&bridge);
        client.event(&reply(peer.drain_calls()[0].request_id, 10, r#"{"error":"busy"}"#));
        client.tick(Duration::ZERO, &mut deadline);
        client.mode_changed("test", "left");
        client.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "nothing before the deadline");
        client.tick(RETRY_FIRST, &mut deadline);
        let mut sent = Vec::new();
        for _ in 0..2 {
            client.flush(&bridge);
            for call in peer.drain_calls() {
                sent.push(serde_json::from_str::<Value>(&call.body).unwrap()["edge"].clone());
                client.event(&reply(call.request_id, 0, r#"{"accepted":true}"#));
            }
        }
        assert!(sent.contains(&json!("left")), "the changed edge is reported after backoff");
    }
}
