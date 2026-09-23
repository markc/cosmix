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
const DISCOVERY_PATH: &str = "input.corners.discovery";

/// One pending write of comp's `input.corners.discovery` (shell design
/// §8.5). Sends are triggered by a Bus connection or by comp being seen
/// registered, never by a timer; a failed write waits for the next trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveryWrite {
    Idle,
    Queued { value: bool, send: bool },
    InFlight { value: bool, id: u64 },
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
    discovery: DiscoveryWrite,
    /// This is the shell's first run: the first discovery write comp
    /// accepts consumes it (see [`Self::take_first_run_written`]).
    first_run: bool,
    first_run_written: bool,
    discovery_failure_logged: bool,
}

pub(crate) fn install(app: &mut App, bus: &mut BusBridgeConfig, service: String) {
    let observer = HotspotObserver::new(service);
    bus.subscriptions.push(observer.topic.clone());
    app.insert_resource(observer)
        .init_resource::<QuoinHotspotSize>();
}

/// Arm the first-run discovery write when the state store says this is the
/// shell's first run (§8.5). A no-op without an installed observer.
pub(crate) fn arm_first_run(app: &mut App, first_run: bool) {
    if first_run && let Some(mut observer) = app.world_mut().get_resource_mut::<HotspotObserver>() {
        observer.arm_first_run_discovery();
    }
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
            discovery: DiscoveryWrite::Idle,
            first_run: false,
            first_run_written: false,
            discovery_failure_logged: false,
        }
    }

    /// First run (no saved shell state yet): ask comp to blink every hotspot
    /// until the first reveal (§8.5). Comp clears the leaf itself at the
    /// first corner engagement. Called once at startup, and only when the
    /// state store reports a first run.
    pub(crate) fn arm_first_run_discovery(&mut self) {
        self.first_run = true;
        self.queue_discovery(true);
    }

    /// HOOK FOR CHUNK 20 (keyboard reveal): a panel revealed from the
    /// keyboard is a reveal comp never sees, so the shell ends the discovery
    /// blink itself by writing `input.corners.discovery = false`. Harmless
    /// when comp already cleared it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn end_discovery_on_keyboard_reveal(&mut self) {
        self.queue_discovery(false);
    }

    fn queue_discovery(&mut self, value: bool) {
        self.discovery = DiscoveryWrite::Queued { value, send: true };
    }

    /// Consume the reply to the in-flight discovery write; false when the
    /// reply belongs to something else.
    fn discovery_reply(&mut self, request_id: u64, result: &Result<BusReply, String>) -> bool {
        let DiscoveryWrite::InFlight { value, id } = self.discovery else {
            return false;
        };
        if id != request_id {
            return false;
        }
        match result {
            Ok(reply) if reply.rc == 0 => {
                self.discovery = DiscoveryWrite::Idle;
                self.discovery_failure_logged = false;
                if self.first_run {
                    self.first_run = false;
                    self.first_run_written = true;
                }
            }
            failed => {
                // Wait for the next trigger; never retry on a clock.
                self.discovery = DiscoveryWrite::Queued { value, send: false };
                if !self.discovery_failure_logged {
                    self.discovery_failure_logged = true;
                    let detail = match failed {
                        Ok(reply) => format!("rc={}", reply.rc),
                        Err(error) => format!("transport error: {error}"),
                    };
                    eprintln!(
                        "QUOIN_DISCOVERY_WRITE_FAILED service={} path={} value={} detail={}",
                        self.service, DISCOVERY_PATH, value, detail
                    );
                }
            }
        }
        true
    }

    /// A trigger (connection, comp registered) releases a queued write.
    fn release_discovery(&mut self) {
        if let DiscoveryWrite::Queued { value, .. } = self.discovery {
            self.discovery = DiscoveryWrite::Queued { value, send: true };
        }
    }

    /// True once, when comp has accepted the first-run write: the caller
    /// records the first run as consumed so it never re-arms.
    pub(crate) fn take_first_run_written(&mut self) -> bool {
        std::mem::take(&mut self.first_run_written)
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
            self.release_discovery();
            return;
        }
        self.present = Some(present);
        // Invalidate any read from the previous service lifetime.
        self.pending = None;
        self.dirty = present;
        if present {
            self.release_discovery();
        }
        *size = QuoinHotspotSize::default();
    }

    pub(crate) fn event(&mut self, event: &BusBridgeEvent, size: &mut QuoinHotspotSize) {
        if let BusBridgeEvent::Reply { request_id, result } = event
            && self.discovery_reply(*request_id, result)
        {
            return;
        }
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
                // A write in flight on the old connection never answers.
                if let DiscoveryWrite::InFlight { value, .. } = self.discovery {
                    self.discovery = DiscoveryWrite::Queued { value, send: true };
                }
                self.release_discovery();
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                self.generation = None;
                self.present = None;
                self.pending = None;
                self.dirty = false;
                *size = QuoinHotspotSize::default();
                if let DiscoveryWrite::InFlight { value, .. } = self.discovery {
                    self.discovery = DiscoveryWrite::Queued { value, send: false };
                }
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
            self.stale = true;
        }
    }

    /// A full queue retries the outstanding event-triggered read. Once sent,
    /// no more requests are issued until another observation requires one.
    pub(crate) fn flush(&mut self, bridge: &BusBridge) {
        self.flush_discovery(bridge);
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
                format!("{}.props.get", self.service),
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

    fn flush_discovery(&mut self, bridge: &BusBridge) {
        let DiscoveryWrite::Queued { value, send: true } = self.discovery else {
            return;
        };
        if self.generation.is_none() {
            return;
        }
        self.next_id += 1;
        let body = json!({ "path": DISCOVERY_PATH, "value": value }).to_string();
        if bridge
            .try_call(
                self.next_id,
                &self.service,
                // Addressed to the selected service, but comp dispatches the
                // literal `comp.*` command whatever its registered name.
                "comp.props.set".to_owned(),
                BTreeMap::new(),
                body,
            )
            .is_ok()
        {
            self.discovery = DiscoveryWrite::InFlight {
                value,
                id: self.next_id,
            };
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
        assert_eq!(calls[0].command, "comp-nested.props.get");
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

    fn discovery_sets(peer: &ctk::bus::TestBusPeer) -> Vec<(u64, Value)> {
        peer.drain_calls()
            .into_iter()
            .filter(|call| call.command == "comp.props.set")
            .map(|call| {
                assert_eq!(call.to, "comp-nested");
                let body: Value = serde_json::from_str(&call.body).unwrap();
                assert_eq!(body["path"], DISCOVERY_PATH);
                (call.request_id, body["value"].clone())
            })
            .collect()
    }

    fn set_reply(observer: &mut HotspotObserver, size: &mut QuoinHotspotSize, id: u64, rc: u8) {
        observer.event(
            &BusBridgeEvent::Reply {
                request_id: id,
                result: Ok(BusReply {
                    rc,
                    body: "{}".into(),
                    result: None,
                }),
            },
            size,
        );
    }

    /// §8.5: on the shell's first run Quoin asks comp to blink the hotspots,
    /// exactly once; the accepted write consumes the first run.
    #[test]
    fn first_run_discovery_is_written_once_and_consumed() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        observer.arm_first_run_discovery();
        observer.flush(&bridge);
        assert!(discovery_sets(&peer).is_empty(), "no connection, no write");
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        let sets = discovery_sets(&peer);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].1, json!(true));
        observer.flush(&bridge);
        assert!(discovery_sets(&peer).is_empty(), "one write in flight");
        assert!(!observer.take_first_run_written());
        set_reply(&mut observer, &mut size, sets[0].0, 0);
        assert!(observer.take_first_run_written());
        assert!(!observer.take_first_run_written(), "reported once");
        // Reconnects and registry receipts never re-request it.
        connect(&mut observer, &mut size, 2);
        observer.presence(&BTreeSet::from(["comp-nested".to_owned()]), &mut size);
        observer.flush(&bridge);
        assert!(discovery_sets(&peer).is_empty());
    }

    /// Comp matches the literal `comp.props.set`; a service-prefixed verb is
    /// `unknown_verb` on any comp not registered as `comp`.
    #[test]
    fn discovery_write_is_literal_comp_props_set_to_the_selected_service() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        observer.arm_first_run_discovery();
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        let sets = peer
            .drain_calls()
            .into_iter()
            .filter(|call| call.command.ends_with("props.set"))
            .collect::<Vec<_>>();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].command, "comp.props.set");
        assert_eq!(sets[0].to, "comp-nested");
    }

    #[test]
    fn first_run_discovery_failure_waits_for_a_trigger_not_a_clock() {
        let (bridge, peer) = test_bridge("shell");
        let mut observer = HotspotObserver::new("comp-nested".into());
        let mut size = QuoinHotspotSize::default();
        observer.arm_first_run_discovery();
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        let first = discovery_sets(&peer);
        set_reply(&mut observer, &mut size, first[0].0, 10);
        for _ in 0..5 {
            observer.flush(&bridge);
        }
        assert!(
            discovery_sets(&peer).is_empty(),
            "a refusal is not retried on a clock"
        );
        assert!(!observer.take_first_run_written());
        // Comp seen registered is the trigger.
        observer.presence(&BTreeSet::from(["comp-nested".to_owned()]), &mut size);
        observer.flush(&bridge);
        let retry = discovery_sets(&peer);
        assert_eq!(retry.len(), 1);
        // A disconnect loses the in-flight write; the reconnect resends it.
        observer.event(
            &BusBridgeEvent::Connection {
                state: BusConnectionState::Disconnected,
                generation: 1,
            },
            &mut size,
        );
        set_reply(&mut observer, &mut size, retry[0].0, 0);
        assert!(
            !observer.take_first_run_written(),
            "a stale reply is ignored"
        );
        connect(&mut observer, &mut size, 2);
        observer.flush(&bridge);
        let resent = discovery_sets(&peer);
        assert_eq!(resent.len(), 1);
        set_reply(&mut observer, &mut size, resent[0].0, 0);
        assert!(observer.take_first_run_written());
    }

    #[test]
    fn not_first_run_writes_nothing_and_keyboard_hook_clears_discovery() {
        let (bridge, peer) = test_bridge("shell");
        let mut app = App::new();
        let mut config = BusBridgeConfig::new("shell", "ws://127.0.0.1:9000");
        install(&mut app, &mut config, "comp-nested".into());
        arm_first_run(&mut app, false);
        let mut observer = app
            .world_mut()
            .remove_resource::<HotspotObserver>()
            .unwrap();
        let mut size = QuoinHotspotSize::default();
        connect(&mut observer, &mut size, 1);
        observer.flush(&bridge);
        assert!(discovery_sets(&peer).is_empty(), "restored state: no blink");
        // Chunk 20's keyboard-reveal hook ends the blink comp cannot see.
        observer.end_discovery_on_keyboard_reveal();
        observer.flush(&bridge);
        let sets = discovery_sets(&peer);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].1, json!(false));
        set_reply(&mut observer, &mut size, sets[0].0, 0);
        assert!(!observer.take_first_run_written(), "not a first run");
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
