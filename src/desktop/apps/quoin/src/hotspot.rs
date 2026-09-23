//! Event-driven mirror of comp's authoritative corner deadzone. Subscribe
//! before the initial read; subsequent reads are triggered only by property
//! events, registry changes, reconnects or delivery gaps, never by a timer.

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use cosmix_shell::chrome::QuoinHotspotSize;
use ctk::bus::{BusBridge, BusBridgeConfig, BusBridgeEvent, BusConnectionState, BusMessage};
use serde_json::Value;

const DEADZONE_PATH: &str = "input.corners.deadzone_px";

#[derive(Resource)]
pub(crate) struct HotspotObserver {
    service: String,
    topic: String,
    generation: Option<u64>,
    present: Option<bool>,
    pending: Option<u64>,
    dirty: bool,
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
            next_id: 0x48_0000_0000,
        }
    }

    pub(crate) fn presence(&mut self, services: &BTreeSet<String>, size: &mut QuoinHotspotSize) {
        let present = services.contains(&self.service);
        if self.present == Some(present) {
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
            BusBridgeEvent::DroppedMessages(_) => self.dirty = self.generation.is_some(),
            BusBridgeEvent::Reply { request_id, result } if self.pending == Some(*request_id) => {
                self.pending = None;
                // A change received during this read requires a fresh read.
                // Do not transiently publish a snapshot already known stale.
                if !self.dirty {
                    *size = result
                        .as_ref()
                        .ok()
                        .filter(|reply| reply.rc == 0)
                        .and_then(|reply| serde_json::from_str::<Value>(&reply.body).ok())
                        .and_then(|body| {
                            body.pointer("/input/corners/deadzone_px")
                                .and_then(Value::as_f64)
                        })
                        .filter(|value| {
                            value.is_finite() && *value > 0.0 && *value <= f32::MAX as f64
                        })
                        .map(|value| QuoinHotspotSize(value as f32))
                        .unwrap_or_default();
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
        let gap = message
            .headers
            .get("gap")
            .is_some_and(|value| value == "true");
        let relevant = serde_json::from_str::<Value>(&message.body)
            .ok()
            .and_then(|body| body.get("path").and_then(Value::as_str).map(str::to_owned))
            .is_some_and(|path| {
                path == DEADZONE_PATH || path == "input.corners" || path == "input"
            });
        if gap || relevant || self.present != Some(true) {
            self.present = Some(true);
            self.dirty = true;
        }
    }

    /// A full queue retries the outstanding event-triggered read. Once sent,
    /// no more requests are issued until another observation requires one.
    pub(crate) fn flush(&mut self, bridge: &BusBridge) {
        if !self.dirty || self.pending.is_some() || self.generation.is_none() {
            return;
        }
        self.next_id += 1;
        if bridge
            .try_call(
                self.next_id,
                &self.service,
                format!("{}.props.get", self.service),
                BTreeMap::new(),
                "{}",
            )
            .is_ok()
        {
            self.pending = Some(self.next_id);
            self.dirty = false;
        }
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

    fn reply(observer: &mut HotspotObserver, size: &mut QuoinHotspotSize, id: u64, value: Value) {
        observer.event(
            &BusBridgeEvent::Reply {
                request_id: id,
                result: Ok(BusReply {
                    rc: 0,
                    body: json!({"input":{"corners":{"deadzone_px": value}}}).to_string(),
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
        assert_eq!(calls[0].command, "comp-nested.props.get");
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
