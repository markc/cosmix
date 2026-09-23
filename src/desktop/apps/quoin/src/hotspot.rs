//! Event-driven mirror of comp's authoritative corner deadzone. Subscribe
//! before the initial read; subsequent reads are triggered only by property
//! events, registry changes, reconnects or delivery gaps, never by a timer.
//! Reads use the comp port's scoped form (`props.get` with a `{"path": ...}`
//! body), so the reply body is the bare value at `input.corners.deadzone_px`.

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use cosmix_shell::chrome::QuoinHotspotSize;
use ctk::bus::{BusBridge, BusBridgeConfig, BusBridgeEvent, BusConnectionState, BusMessage, BusReply};
use serde_json::{Value, json};

const DEADZONE_PATH: &str = "input.corners.deadzone_px";
/// Comp verbs are literal `comp.*` commands addressed to the selected service
/// (`to`); only topics carry the service name.
pub(crate) const COMP_PROPS_GET: &str = "comp.props.get";

/// Comp reports lost observation records in the frame's body
/// (`{"gap":true,"lost_count":N,"cause":...}`), on the topic that lost them;
/// there is no `gap` header.
pub(crate) fn is_comp_gap(body: &Value) -> bool {
    body.get("gap").and_then(Value::as_bool) == Some(true)
}

#[derive(Resource)]
pub(crate) struct HotspotObserver {
    service: String,
    topic: String,
    generation: Option<u64>,
    present: Option<bool>,
    pending: Option<u64>,
    dirty: bool,
    /// A deadzone change (or an unknown one: gap, dropped messages) landed
    /// while a read was in flight, so its reply is known stale and is not
    /// published. Registry receipts set only `dirty`: they queue one follow-up
    /// read without discarding the reply, so unrelated registry churn cannot
    /// starve the inset.
    stale: bool,
    /// One `QUOIN_HOTSPOT_READ_FAILED` notice per run of failed reads; a
    /// successful read re-arms it so a later failure is noticed again.
    read_failure_logged: bool,
    next_id: u64,
}

pub(crate) fn install(app: &mut App, bus: &mut BusBridgeConfig, service: String) {
    let observer = HotspotObserver::new(service);
    bus.subscriptions.push(observer.topic.clone());
    app.insert_resource(observer)
        .init_resource::<QuoinHotspotSize>();
}

impl HotspotObserver {
    fn new(service: String) -> Self {
        Self {
            topic: format!("{service}.props.changed"),
            service,
            generation: None,
            present: None,
            pending: None,
            dirty: false,
            stale: false,
            read_failure_logged: false,
            next_id: 0x48_0000_0000,
        }
    }

    pub(crate) fn presence(&mut self, services: &BTreeSet<String>, size: &mut QuoinHotspotSize) {
        let present = services.contains(&self.service);
        if self.present == Some(present) {
            if !present {
                return;
            }
            // No transition, yet a fresh registry observation reports comp
            // registered: a re-registration receipt. A comp that left and
            // re-registered between observations (or inside a leave/register
            // burst noded suppressed) publishes no initial `props.changed` —
            // comp emits changes only — so this lifetime's value is re-read.
            self.dirty = true;
            return;
        }
        self.present = Some(present);
        // Invalidate any read from the previous service lifetime.
        self.pending = None;
        self.dirty = present;
        *size = QuoinHotspotSize::default();
    }

    pub(crate) fn event(&mut self, event: &BusBridgeEvent, size: &mut QuoinHotspotSize) {
        match event {
            BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            } => {
                self.generation = Some(*generation);
                self.present = None;
                self.pending = None;
                self.dirty = true;
                *size = QuoinHotspotSize::default();
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                self.generation = None;
                self.present = None;
                self.pending = None;
                self.dirty = false;
                *size = QuoinHotspotSize::default();
            }
            BusBridgeEvent::DroppedMessages(_) => {
                self.dirty = self.generation.is_some();
                self.stale = self.dirty;
            }
            BusBridgeEvent::Reply { request_id, result } if self.pending == Some(*request_id) => {
                self.pending = None;
                // A deadzone change received during this read requires a fresh
                // read; do not transiently publish a snapshot known stale.
                if !self.stale {
                    match scoped_deadzone(result) {
                        Ok(value) => {
                            *size = QuoinHotspotSize(value as f32);
                            self.read_failure_logged = false;
                        }
                        Err(detail) => {
                            *size = QuoinHotspotSize::default();
                            if !self.read_failure_logged {
                                self.read_failure_logged = true;
                                eprintln!(
                                    "QUOIN_HOTSPOT_READ_FAILED service={} path={} detail={} using_default_px={}",
                                    self.service,
                                    DEADZONE_PATH,
                                    detail,
                                    QuoinHotspotSize::default().0
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn message(&mut self, message: &BusMessage) {
        if self.generation != Some(message.connection_generation)
            || message.topic() != Some(self.topic.as_str())
        {
            return;
        }
        let body = serde_json::from_str::<Value>(&message.body).ok();
        let gap = body.as_ref().is_some_and(is_comp_gap);
        let relevant = body
            .as_ref()
            .and_then(|body| body.get("path").and_then(Value::as_str))
            .is_some_and(|path| {
                path == DEADZONE_PATH || path == "input.corners" || path == "input"
            });
        if gap || relevant || self.present != Some(true) {
            self.present = Some(true);
            self.dirty = true;
            self.stale = true;
        }
    }

    /// A full queue retries the outstanding event-triggered read. Once sent,
    /// no more requests are issued until another observation requires one.
    pub(crate) fn flush(&mut self, bridge: &BusBridge) {
        if !self.dirty || self.pending.is_some() || self.generation.is_none() {
            return;
        }
        self.next_id += 1;
        // The comp port's scoped read: `props.get` with a `path` body answers
        // with only that leaf, instead of the whole comp snapshot.
        let body = json!({ "path": DEADZONE_PATH }).to_string();
        if bridge
            .try_call(
                self.next_id,
                &self.service,
                COMP_PROPS_GET,
                BTreeMap::new(),
                body,
            )
            .is_ok()
        {
            self.pending = Some(self.next_id);
            self.dirty = false;
            self.stale = false;
        }
    }
}

/// Parse the scoped reply. The body is the bare value at [`DEADZONE_PATH`]
/// (comp's `props.get` answers a `path`-scoped request with that leaf alone),
/// so there is no JSON pointer to walk — anything that is not a usable
/// finite positive number is a failed read.
fn scoped_deadzone(result: &Result<BusReply, String>) -> Result<f64, String> {
    let reply = result
        .as_ref()
        .map_err(|error| format!("transport error: {error}"))?;
    if reply.rc != 0 {
        return Err(format!("rc={}", reply.rc));
    }
    let body: Value = serde_json::from_str(&reply.body)
        .map_err(|error| format!("unparseable body: {error}"))?;
    let value = body
        .as_f64()
        .ok_or_else(|| "body is not the scoped numeric value".to_owned())?;
    if value.is_finite() && value > 0.0 && value <= f32::MAX as f64 {
        Ok(value)
    } else {
        Err(format!("invalid value: {value}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::bus::{BusReply, test_bridge};
    use serde_json::json;

    fn connect(observer: &mut HotspotObserver, size: &mut QuoinHotspotSize, generation: u64) {
        observer.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            },
            size,
        );
    }

    /// A scoped-read reply: the body is the bare value at the requested
    /// path, the shape comp's `props.get` answers a `{"path": ...}` body with.
    fn reply(observer: &mut HotspotObserver, size: &mut QuoinHotspotSize, id: u64, value: Value) {
        observer.event(
            &BusBridgeEvent::Reply {
                request_id: id,
                result: Ok(BusReply {
                    rc: 0,
                    body: value.to_string(),
                    result: None,
                }),
            },
            size,
        );
    }

    fn change(generation: u64) -> BusMessage {
        BusMessage {
            connection_generation: generation,
            from: "comp-nested".into(),
            command: "props.changed".into(),
            body: json!({"path": DEADZONE_PATH, "new": 24.0}).to_string(),
            headers: BTreeMap::from([("topic".into(), "comp-nested.props.changed".into())]),
        }
    }

    #[test]
    fn hotspot_subscription_reads_selected_comp_and_never_polls() {
        let mut app = App::new();
        let mut config = BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        install(&mut app, &mut config, "comp-nested".into());
        assert_eq!(config.subscriptions, ["comp-nested.props.changed"]);
        let mut observer = app
            .world_mut()
            .remove_resource::<HotspotObserver>()
            .unwrap();
        let (bridge, peer) = test_bridge("shell");
        let mut size = QuoinHotspotSize::default();
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to, "comp-nested");
        // A literal comp verb addressed to the selected service.
        assert_eq!(calls[0].command, "comp.props.get");
        assert_eq!(calls[0].body, r#"{"path":"input.corners.deadzone_px"}"#);
        reply(&mut observer, &mut size, calls[0].request_id, json!(24.0));
        assert_eq!(size.0, 24.0);
        for _ in 0..10 {
            observer.flush(&bridge);
        }
        assert!(peer.drain_calls().is_empty());
        observer.message(&change(1));
        observer.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        reply(&mut observer, &mut size, calls[0].request_id, json!(40.0));
        assert_eq!(size.0, 40.0);
    }

    #[test]
    fn hotspot_reconnect_gap_and_absent_comp_recover_without_polling() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        let old = peer.drain_calls()[0].request_id;
        connect(&mut observer, &mut size, 2);
        reply(&mut observer, &mut size, old, json!(99.0));
        assert_eq!(size.0, QuoinHotspotSize::default().0);
        observer.flush(&bridge);
        let id = peer.drain_calls()[0].request_id;
        // An event during the read invalidates that reply and coalesces one read.
        observer.message(&change(2));
        reply(&mut observer, &mut size, id, json!(99.0));
        assert_eq!(size.0, QuoinHotspotSize::default().0);
        observer.flush(&bridge);
        reply(
            &mut observer,
            &mut size,
            peer.drain_calls()[0].request_id,
            json!(30.0),
        );
        assert_eq!(size.0, 30.0);
        observer.message(&change(1));
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        observer.event(&BusBridgeEvent::DroppedMessages(1), &mut size);
        observer.flush(&bridge);
        assert_eq!(peer.drain_calls().len(), 1);
        observer.presence(&BTreeSet::new(), &mut size);
        assert_eq!(size.0, QuoinHotspotSize::default().0);
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        observer.presence(&BTreeSet::from(["comp-nested".into()]), &mut size);
        observer.flush(&bridge);
        reply(
            &mut observer,
            &mut size,
            peer.drain_calls()[0].request_id,
            json!(18.0),
        );
        assert_eq!(size.0, 18.0);
    }

    /// Comp's gap frame, byte for byte: the marker is in the body, and the
    /// headers carry only the topic, command and last lost sequence.
    #[test]
    fn hotspot_comp_gap_body_forces_a_reread() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        connect(&mut observer, &mut size, 1);
        observer.presence(&BTreeSet::from(["comp-nested".to_owned()]), &mut size);
        observer.flush(&bridge);
        reply(&mut observer, &mut size, peer.drain_calls()[0].request_id, json!(24.0));
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        observer.message(&BusMessage {
            connection_generation: 1,
            from: "comp-nested".into(),
            command: "props.changed".into(),
            body: r#"{"gap":true,"lost_count":3,"cause":"outbox.overflow"}"#.into(),
            headers: BTreeMap::from([
                ("topic".into(), "comp-nested.props.changed".into()),
                ("command".into(), "props.changed".into()),
                ("event_seq".into(), "41".into()),
            ]),
        });
        observer.flush(&bridge);
        assert_eq!(peer.drain_calls().len(), 1, "lost changes are re-read");
    }

    /// A registered→registered observation is a re-registration receipt: no
    /// presence transition, but comp may be a new lifetime that publishes no
    /// initial `props.changed`, so exactly one re-read fires. Observations
    /// that do not report comp registered trigger none.
    #[test]
    fn hotspot_registry_receipt_without_transition_rereads_once() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        connect(&mut observer, &mut size, 1);
        let registered = BTreeSet::from(["comp-nested".to_owned()]);
        // First observation: the None→registered transition.
        observer.presence(&registered, &mut size);
        observer.flush(&bridge);
        reply(&mut observer, &mut size, peer.drain_calls()[0].request_id, json!(24.0));
        assert_eq!(size.0, 24.0);
        observer.presence(&registered, &mut size);
        // The last good value stands until the receipt's read lands.
        assert_eq!(size.0, 24.0);
        observer.flush(&bridge);
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        reply(&mut observer, &mut size, calls[0].request_id, json!(30.0));
        assert_eq!(size.0, 30.0);
        // No further observation: nothing fires.
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        // Unrelated observations (comp not reported) trigger none: the
        // transition to absent invalidates, and absent→absent is silent.
        let unrelated = BTreeSet::from(["other-service".to_owned()]);
        observer.presence(&unrelated, &mut size);
        assert_eq!(size.0, QuoinHotspotSize::default().0);
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
        observer.presence(&unrelated, &mut size);
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty());
    }

    /// The scoped reply contract: the bare leaf value parses, and transport
    /// errors, non-zero rc, non-numeric bodies and out-of-range values are
    /// failed reads.
    #[test]
    fn hotspot_scoped_reply_parses_bare_value_only() {
        let ok = |rc: u8, body: &str| {
            Ok(BusReply {
                rc,
                body: body.to_owned(),
                result: None,
            })
        };
        assert_eq!(scoped_deadzone(&ok(0, "18")), Ok(18.0));
        assert_eq!(scoped_deadzone(&ok(0, "18.5")), Ok(18.5));
        assert!(scoped_deadzone(&ok(10, r#"{"error":"unknown_path"}"#)).is_err());
        assert!(scoped_deadzone(&ok(0, r#""24""#)).is_err());
        assert!(scoped_deadzone(&ok(0, r#"{"input":{}}"#)).is_err());
        assert!(scoped_deadzone(&ok(0, "-1")).is_err());
        assert!(scoped_deadzone(&ok(0, "0")).is_err());
        assert!(scoped_deadzone(&ok(0, "1e100")).is_err());
        assert!(scoped_deadzone(&Err("comp unavailable".into())).is_err());
    }

    #[test]
    fn hotspot_invalid_or_failed_observation_uses_default() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize(24.0);
        connect(&mut observer, &mut size, 1);
        for value in [Value::Null, json!(-1), json!(0), json!("24"), json!(1e100)] {
            observer.message(&change(1));
            observer.flush(&bridge);
            reply(
                &mut observer,
                &mut size,
                peer.drain_calls()[0].request_id,
                value,
            );
            assert_eq!(size.0, QuoinHotspotSize::default().0);
        }
        observer.message(&change(1));
        observer.flush(&bridge);
        observer.event(
            &BusBridgeEvent::Reply {
                request_id: peer.drain_calls()[0].request_id,
                result: Err("comp unavailable".into()),
            },
            &mut size,
        );
        observer.flush(&bridge);
        assert!(peer.drain_calls().is_empty(), "failed reads await an event");
        assert_eq!(size.0, QuoinHotspotSize::default().0);
    }
}
