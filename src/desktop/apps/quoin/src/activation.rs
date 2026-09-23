//! Named activation (panel doc §6): the `shell.sub.activate` verb, gated on
//! comp's holder plane, and the observations that aim it at an output
//! (shell doc §5).
//!
//! A hidden edge's activation is a transient reveal held by a compositor
//! focus hold (see [`crate::holders`]). Without the holder plane nothing
//! could hold it — a reveal the local rules conceal would vanish while the
//! user types — so until comp reports the plane the verb is refused with
//! `ACTIVATION_UNAVAILABLE`, never a silent no-op and never a local reveal.
//!
//! Targeting follows keyboard focus and the pointer through comp's
//! `focus.changed` topic and one `comp.props.get` round per change: the
//! `focus` subtree names the focused surface and the one under the pointer,
//! then each surface's `output` leaf. Event-driven only; comp's pointer
//! stream needs a renewed lease, so it is deliberately not used.

use std::collections::BTreeMap;
use std::time::Duration;

use bevy::prelude::*;
use cosmix_shell::core::{OutputKey, SubPanelRegistry, SubPanelRegistryError, keyboard_target_output};
use cosmix_shell::runtime::{ShellCommand, ShellFrame, ShellSemanticVerb, semantic_shell_command};
use ctk::app_control::verify_caller_provenance;
use ctk::bus::{BusBridge, BusBridgeConfig, BusBridgeEvent, BusConnectionState, BusMessage, InboundRequest};
use serde_json::{Value, json};

use crate::bus_service::{argument, edge_name};
use crate::hotspot::COMP_PROPS_GET;

const FOCUS_TOPIC_SUFFIX: &str = "focus.changed";
pub(crate) const ACTIVATION_UNAVAILABLE: &str = "ACTIVATION_UNAVAILABLE";
pub(crate) const HOLDER_PLANE_UNAVAILABLE: &str = "compositor holder plane not available";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Focused,
    Pointer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Focus,
    Output(Slot),
}

/// Where the user is, as far as comp's observations say (shell doc §5).
#[derive(Resource)]
pub(crate) struct ActivationTargets {
    service: String,
    generation: Option<u64>,
    read_needed: bool,
    /// The current round's reads in flight; a new round forgets the old.
    reads: BTreeMap<u64, Read>,
    /// Surface output reads owed by the latest focus reply.
    queued: Vec<(Slot, u64)>,
    focused: Option<OutputKey>,
    pointer: Option<OutputKey>,
    next_id: u64,
}

pub(crate) fn install(app: &mut App, bus: &mut BusBridgeConfig, service: String) {
    let topic = format!("{service}.{FOCUS_TOPIC_SUFFIX}");
    if !bus.subscriptions.contains(&topic) {
        bus.subscriptions.push(topic);
    }
    app.insert_resource(ActivationTargets::new(service));
}

impl ActivationTargets {
    fn new(service: String) -> Self {
        Self {
            service, generation: None, read_needed: false,
            reads: BTreeMap::new(), queued: Vec::new(), focused: None, pointer: None,
            next_id: 0x41_0000_0000,
        }
    }

    /// The output an activation aims at: the focused window's, else the
    /// pointer's. `None` until comp has answered.
    pub(crate) fn target(&self) -> Option<&OutputKey> {
        // The same shell doc §5 rule keyboard actions follow.
        keyboard_target_output(self.focused.as_ref(), self.pointer.as_ref())
    }

    /// Forget what comp said and start a fresh round on the next flush.
    fn reread(&mut self) {
        self.read_needed = self.generation.is_some();
        self.reads.clear();
        self.queued.clear();
    }

    pub(crate) fn event(&mut self, event: &BusBridgeEvent) {
        match event {
            BusBridgeEvent::Connection { state: BusConnectionState::Connected, generation } => {
                self.generation = Some(*generation);
                self.reread();
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                self.generation = None;
                self.focused = None;
                self.pointer = None;
                self.reread();
            }
            // Lost messages may include a focus change.
            BusBridgeEvent::DroppedMessages(_) => self.reread(),
            BusBridgeEvent::Reply { request_id, result } => {
                let Some(read) = self.reads.remove(request_id) else { return };
                let body = result.as_ref().ok().filter(|reply| reply.rc == 0)
                    .and_then(|reply| serde_json::from_str::<Value>(&reply.body).ok());
                match read {
                    Read::Focus => {
                        // A failed read leaves both unknown until the next trigger.
                        let body = body.unwrap_or(Value::Null);
                        for (slot, field) in [(Slot::Focused, "keyboard"), (Slot::Pointer, "pointer")] {
                            match body[field].as_u64() {
                                Some(surface) => self.queued.push((slot, surface)),
                                None => *self.slot(slot) = None,
                            }
                        }
                    }
                    Read::Output(slot) => {
                        *self.slot(slot) = body.as_ref().and_then(Value::as_str)
                            .and_then(|name| OutputKey::new(name).ok());
                    }
                }
            }
            _ => {}
        }
    }

    fn slot(&mut self, slot: Slot) -> &mut Option<OutputKey> {
        match slot {
            Slot::Focused => &mut self.focused,
            Slot::Pointer => &mut self.pointer,
        }
    }

    /// A focus change (or a gap that may have hidden one) starts a new round.
    pub(crate) fn message(&mut self, message: &BusMessage) {
        if self.generation != Some(message.connection_generation) {
            return;
        }
        let topic = format!("{}.{FOCUS_TOPIC_SUFFIX}", self.service);
        if message.topic() == Some(topic.as_str()) {
            self.reread();
        }
    }

    /// Sends the round's reads. A full outbound queue keeps them owed for the
    /// next update; nothing is retried on a clock.
    pub(crate) fn flush(&mut self, bridge: &BusBridge) {
        if self.generation.is_none() {
            return;
        }
        if self.read_needed {
            self.next_id += 1;
            if bridge.try_call(self.next_id, &self.service, COMP_PROPS_GET, BTreeMap::new(),
                json!({"path":"focus"}).to_string()).is_err() {
                return;
            }
            self.read_needed = false;
            self.reads.insert(self.next_id, Read::Focus);
        }
        while let Some(&(slot, surface)) = self.queued.first() {
            self.next_id += 1;
            if bridge.try_call(self.next_id, &self.service, COMP_PROPS_GET, BTreeMap::new(),
                json!({"path":format!("surfaces.s{surface}.output")}).to_string()).is_err() {
                return;
            }
            self.queued.remove(0);
            self.reads.insert(self.next_id, Read::Output(slot));
        }
    }
}

/// `shell.sub.activate name=<name>` (panel doc §6). Refusals, in order: an
/// unattested caller or a stale Quoin connection (the fences every sub-panel
/// verb keeps), a missing name, an unregistered name (the same refusal as
/// `sub.remove` — activation never creates), and then the capability gate.
/// The name is the address: its seat supplies the edge, owner and receipt,
/// and the Model stage applies the activation only while that exact
/// registration stands, binding hidden-versus-persistent at Model time.
///
/// v1 Quoin runs one output and every seat lives on it, so the sub-panel is
/// shown on its seat's output; the reply reports the §5 `target` so a caller
/// can see when the user is on an output this Quoin does not run.
pub(crate) fn dispatch_activate(
    request: &InboundRequest,
    frame: &ShellFrame,
    registry: &SubPanelRegistry,
    capable: bool,
    target: Option<&OutputKey>,
    live_generation: Option<u64>,
    at: Duration,
) -> (u8, String, Option<ShellCommand>) {
    if let Err(error) = verify_caller_provenance(request) {
        let error = format!("sub-panel caller provenance: {error:?}");
        return (10, json!({"error":error}).to_string(), None);
    }
    if live_generation.is_some_and(|generation| generation != request.connection_generation) {
        return (10, json!({"error":"sub-panel request belongs to a stale Quoin connection"})
            .to_string(), None);
    }
    let Some(name) = argument(request, "name").filter(|name| !name.trim().is_empty()) else {
        return (10, json!({"error":"sub.activate requires a name argument"}).to_string(), None);
    };
    let Some(seat) = registry.seat(&name) else {
        let error = SubPanelRegistryError::Unknown(name);
        return (10, json!({"error":error.to_string()}).to_string(), None);
    };
    if !capable {
        return (10, json!({
            "error_code":ACTIVATION_UNAVAILABLE,
            "error":format!("named activation unavailable: {HOLDER_PLANE_UNAVAILABLE}"),
            "reason":HOLDER_PLANE_UNAVAILABLE,
            "name":name,
        }).to_string(), None);
    }
    let body = json!({
        "accepted":true, "name":name, "edge":edge_name(seat.edge),
        "output":seat.output.as_str(), "target":target.map(OutputKey::as_str),
    });
    let command = semantic_shell_command(
        frame.geometry.output.clone(),
        at,
        seat.edge,
        ShellSemanticVerb::SubActivate {
            name,
            owner: seat.owner.clone(),
            accepted_at: seat.accepted_at,
        },
    );
    (0, body.to_string(), Some(command))
}
