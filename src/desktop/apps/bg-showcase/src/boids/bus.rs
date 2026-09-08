//! Native ABP scene synchronisation and wallpaper status port.
//!
//! Notifications are invalidations, not a second mutable window registry.
//! One coherent snapshot request may be in flight; bursts coalesce at 30 Hz.
//! A low-rate reconciliation also detects a lost final notification or a comp
//! restart that does not restart noded. Nothing logs snapshot payloads.

use crate::boids::geometry::Geometry;
use crate::boids::pointer::ScenePointer;
use bevy::prelude::*;
use cosmix_shell_host::scene::{SceneControl, SceneMetrics, SceneUpdateDeadline, SceneWake};
use ctk::bus::{
    BusBridge, BusBridgeConfig, BusBridgeEvent, BusBridgePlugin, BusConnectionState, BusWorkerWake,
    resolve_noded_url,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const QUERY_INTERVAL: Duration = Duration::from_millis(34);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const MAX_AGE: Duration = Duration::from_millis(2500);
const QUERY_WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Resource, Default)]
pub struct SceneGeometry(pub Option<Geometry>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Query {
    Watch,
    Snapshot,
    PointerWatch,
}

#[derive(Resource)]
pub struct SceneBus {
    generation: Option<u64>,
    pending: Option<(u64, u64, Query)>,
    pending_since: Option<Instant>,
    next_id: u64,
    watched: bool,
    dirty: bool,
    next_query: Instant,
    refreshed: Option<Instant>,
    pointer_renew: Instant,
    pointer_ready: bool,
    pointer_waiting: Option<(ctk::bus::BusMessage, Instant)>,
    pub requests: u64,
    pub notifications: u64,
    pub resets: u64,
    pub bytes: u64,
    pub simulation_steps: u64,
    pub simulation_ns: u64,
    queue_peaks: BTreeMap<&'static str, usize>,
    latest_peak: usize,
    dropped_messages: u64,
}
impl Default for SceneBus {
    fn default() -> Self {
        Self {
            generation: None,
            pending: None,
            pending_since: None,
            next_id: 1,
            watched: false,
            dirty: true,
            next_query: Instant::now(),
            refreshed: None,
            pointer_renew: Instant::now(),
            pointer_ready: false,
            pointer_waiting: None,
            requests: 0,
            notifications: 0,
            resets: 0,
            bytes: 0,
            simulation_steps: 0,
            simulation_ns: 0,
            queue_peaks: BTreeMap::new(),
            latest_peak: 0,
            dropped_messages: 0,
        }
    }
}
impl SceneBus {
    fn observe_queues(&mut self, bridge: &BusBridge) -> ctk::bus::BusQueueSnapshot {
        let snapshot = bridge.queue_snapshot();
        for queue in &snapshot.queues {
            let peak = self.queue_peaks.entry(queue.name).or_default();
            *peak = (*peak).max(queue.depth);
        }
        self.latest_peak = self.latest_peak.max(snapshot.latest_topics);
        snapshot
    }

    fn queue_status(&mut self, bridge: &BusBridge) -> serde_json::Value {
        let snapshot = self.observe_queues(bridge);
        let queues: BTreeMap<_, _> = snapshot
            .queues
            .into_iter()
            .map(|queue| {
                (
                    queue.name,
                    json!({"depth":queue.depth,"capacity":queue.capacity,
                "sampled_peak":self.queue_peaks[queue.name]}),
                )
            })
            .collect();
        json!({"channels":queues,"latest_topics":{"depth":snapshot.latest_topics,
            "sampled_peak":self.latest_peak}})
    }

    fn invalidate(&mut self, geometry: &mut SceneGeometry) {
        geometry.0 = None;
        self.refreshed = None;
        self.pending = None;
        self.pending_since = None;
        self.watched = false;
        self.pointer_ready = false;
        self.pointer_waiting = None;
        self.dirty = true;
        self.resets = self.resets.saturating_add(1);
    }
}

pub fn configure(app: &mut App) {
    configure_named(app, "wallpaper");
}

#[derive(Resource, Default)]
pub struct OtherRequests(pub Vec<ctk::bus::InboundRequest>);

pub fn configure_named(app: &mut App, service: &str) {
    let mut config = BusBridgeConfig::new(service, resolve_noded_url());
    config.provenance = ctk::bus::provenance_from_build(cosmix_buildinfo::build_info!());
    config.worker_wake = Some(BusWorkerWake::new(
        app.world().resource::<SceneWake>().callback(),
    ));
    config.subscriptions = vec!["comp.props.changed".into(), "comp.pointer.changed".into()];
    config.latest_topics = vec!["comp.pointer.changed".into()];
    config.inbound_prefixes = vec!["wallpaper.".into()];
    if service == "bg-showcase" {
        config
            .inbound_prefixes
            .extend(["background.".into(), "boing.".into()]);
    }
    config.outbound_capacity = 16;
    config.event_capacity = 32;
    config.message_capacity = 64;
    config.max_messages_per_frame = 64;
    app.world_mut().resource_mut::<SceneControl>().paused = true;
    app.init_resource::<SceneGeometry>()
        .init_resource::<OtherRequests>()
        .init_resource::<SceneBus>()
        .init_resource::<ScenePointer>()
        .add_plugins(BusBridgePlugin::new(config));
}

#[allow(clippy::too_many_arguments)]
pub fn service(
    bridge: Res<BusBridge>,
    mut state: ResMut<SceneBus>,
    mut geometry: ResMut<SceneGeometry>,
    mut pointer: ResMut<ScenePointer>,
    presentation: (
        ResMut<SceneControl>,
        ResMut<SceneUpdateDeadline>,
        Res<SceneMetrics>,
    ),
    mut preferences: ResMut<crate::boids::preferences::PreferenceStore>,
    mut other: ResMut<OtherRequests>,
    active: Option<Res<crate::boids::Active>>,
) {
    let (mut control, mut deadline, metrics) = presentation;
    let now = Instant::now();
    state.observe_queues(&bridge);
    bridge.claim_inbound("wallpaper service");
    for event in bridge.drain_events() {
        match event {
            BusBridgeEvent::Connection {
                state: BusConnectionState::Connected,
                generation,
            } => {
                state.invalidate(&mut geometry);
                state.generation = Some(generation);
                state.next_query = now;
                state.pointer_renew = now;
                preferences.invalidate_subscribers();
            }
            BusBridgeEvent::Connection { .. } | BusBridgeEvent::Fatal(_) => {
                state.invalidate(&mut geometry);
                state.generation = None;
            }
            BusBridgeEvent::DroppedMessages(count) => {
                state.dropped_messages = state.dropped_messages.saturating_add(count as u64);
                state.invalidate(&mut geometry);
                state.next_query = now + QUERY_INTERVAL;
            }
            BusBridgeEvent::Reply { request_id, result } => {
                let Some((id, generation, kind)) = state.pending else {
                    continue;
                };
                if id != request_id || state.generation != Some(generation) {
                    continue;
                }
                state.pending = None;
                state.pending_since = None;
                if kind == Query::PointerWatch {
                    state.pointer_ready = result.as_ref().is_ok_and(|reply| {
                        reply.rc == 0
                            && reply.body.len() <= 4096
                            && serde_json::from_str::<serde_json::Value>(&reply.body)
                                .ok()
                                .is_some_and(|v| {
                                    v["version"] == 1
                                        && v["topic"] == "comp.pointer.changed"
                                        && v["lease_ms"].as_u64().is_some_and(|ms| ms >= 2000)
                                })
                    });
                    state.pointer_renew = now + RECONCILE_INTERVAL;
                    if !state.pointer_ready {
                        pointer.clear();
                    }
                    continue;
                }
                let reply = match result {
                    Ok(reply) if reply.rc == 0 => reply,
                    _ => {
                        state.invalidate(&mut geometry);
                        state.next_query = now + RECONCILE_INTERVAL;
                        continue;
                    }
                };
                state.bytes = state.bytes.saturating_add(reply.body.len() as u64);
                if kind == Query::Watch {
                    let valid = reply.body.len() <= 4096
                        && serde_json::from_str::<serde_json::Value>(&reply.body)
                            .ok()
                            .is_some_and(|v| {
                                v["topic"] == "comp.props.changed"
                                    && v["event_seq"].as_u64().is_some()
                                    && v["lost_count"].as_u64().is_some()
                            });
                    if valid {
                        state.watched = true;
                        state.dirty = true;
                        state.next_query = now;
                    } else {
                        state.invalidate(&mut geometry);
                        state.next_query = now + RECONCILE_INTERVAL;
                    }
                } else {
                    match Geometry::decode(&reply.body) {
                        Ok(snapshot)
                            if geometry.0.as_ref().is_none_or(|old| {
                                old.instance != snapshot.instance
                                    || old.sequence <= snapshot.sequence
                            }) =>
                        {
                            geometry.0 = Some(snapshot);
                            state.refreshed = Some(now);
                        }
                        _ => {
                            state.invalidate(&mut geometry);
                            state.next_query = now + RECONCILE_INTERVAL;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for message in bridge.drain_messages() {
        if state.generation == Some(message.connection_generation)
            && message.topic() == Some("comp.props.changed")
        {
            // noded reserves publication of comp.props.changed to comp. The
            // inner message's `from` is opaque and may be absent; it is not
            // the publisher identity. This message only invalidates state:
            // all geometry still comes from a directed, correlated comp call.
            state.notifications = state.notifications.saturating_add(1);
            state.bytes = state.bytes.saturating_add(message.body.len() as u64);
            state.dirty = true;
        }
    }
    if state
        .pending_since
        .is_some_and(|at| now.saturating_duration_since(at) >= QUERY_WATCHDOG)
    {
        // A dropped terminal reply must not leave the app waiting forever.
        // Request IDs are never reused, so a late reply cannot seed this epoch.
        if state
            .pending
            .is_some_and(|(_, _, kind)| kind == Query::PointerWatch)
        {
            state.pending = None;
            state.pending_since = None;
            state.pointer_ready = false;
            state.pointer_renew = now + RECONCILE_INTERVAL;
            pointer.clear();
        } else {
            state.invalidate(&mut geometry);
        }
        state.next_query = now + RECONCILE_INTERVAL;
    }
    if state
        .refreshed
        .is_some_and(|at| now.saturating_duration_since(at) >= MAX_AGE)
    {
        geometry.0 = None;
        state.refreshed = None;
        state.dirty = true;
    }
    let refresh_due = state.refreshed.map_or(now, |at| at + RECONCILE_INTERVAL);
    pointer.bind(state.generation, geometry.0.as_ref());
    // Control replies and telemetry have independent connections. Retain one
    // sample while admission is pending, including its reception time so a
    // delayed acknowledgement cannot make an expired sample fresh again.
    let pointer_admitting = state
        .pending
        .is_some_and(|(_, _, kind)| kind == Query::PointerWatch);
    for message in bridge.drain_latest_messages() {
        if state.generation == Some(message.connection_generation)
            && message.topic() == Some("comp.pointer.changed")
            && message.body.len() <= 4096
            && (state.pointer_ready || pointer_admitting)
            && message.headers.get("broker_origin").map(String::as_str) == Some("local")
            && message.headers.get("broker_service").map(String::as_str) == Some("comp")
        {
            // noded stamps the reserved topic owner's identity. Checking only
            // the topic would also accept forged directed deliveries.
            state.bytes = state.bytes.saturating_add(message.body.len() as u64);
            state.pointer_waiting = Some((message, now));
        }
    }
    if state.pointer_ready {
        if let Some((message, received)) = state.pointer_waiting.take()
            && let Some(scene) = geometry.0.as_ref()
        {
            pointer.accept(
                message.connection_generation,
                scene,
                &message.body,
                received,
            );
        }
    } else if !pointer_admitting {
        state.pointer_waiting = None;
    }
    pointer.expire(now);
    let pointer_enabled = preferences.current.enabled
        && !preferences.current.paused
        && geometry.0.as_ref().is_some_and(|s| !s.locked);
    let pointer_due = pointer_enabled && now >= state.pointer_renew;
    if state.generation.is_some()
        && state.pending.is_none()
        && now >= state.next_query
        && (!state.watched || state.dirty || now >= refresh_due || pointer_due)
    {
        let kind = if !state.watched {
            Query::Watch
        } else if now >= refresh_due || geometry.0.is_none() {
            Query::Snapshot
        } else if pointer_due {
            // Renew ahead of ordinary geometry invalidations so a busy
            // window drag cannot starve the short pointer lease.
            Query::PointerWatch
        } else {
            Query::Snapshot
        };
        let verb = match kind {
            Query::Watch => "comp.props.watch",
            Query::Snapshot => "comp.props.get",
            Query::PointerWatch => "comp.pointer.watch",
        };
        let id = state.next_id;
        if let Some(next) = id.checked_add(1) {
            state.next_id = next;
            if bridge
                .try_call(id, "comp", verb, BTreeMap::new(), "{}")
                .is_ok()
            {
                state.pending = Some((id, state.generation.unwrap(), kind));
                state.pending_since = Some(now);
                state.requests = state.requests.saturating_add(1);
                if kind != Query::PointerWatch {
                    state.dirty = false;
                }
                state.next_query = now + QUERY_INTERVAL;
            } else {
                state.next_query = now + RECONCILE_INTERVAL;
            }
        } else {
            state.generation = None;
            geometry.0 = None;
        }
    }
    control.paused = !preferences.current.enabled
        || preferences.current.paused
        || geometry.0.as_ref().is_none_or(|s| s.locked);
    // No render-frame dependency for expiry/retry/reconciliation. A pending
    // call is completed or timed out by the existing bounded Bus worker.
    deadline.0 = if state.generation.is_none() {
        None
    } else if state.pending.is_some() {
        let watchdog = state.pending_since.expect("pending query has a deadline") + QUERY_WATCHDOG;
        Some(
            state
                .refreshed
                .map_or(watchdog, |at| watchdog.min(at + MAX_AGE)),
        )
    } else {
        Some(state.next_query.max(if state.dirty || !state.watched {
            now
        } else {
            if pointer_enabled {
                refresh_due.min(state.pointer_renew)
            } else {
                refresh_due
            }
        }))
    };
    if let Some(expiry) = pointer.deadline() {
        deadline.0 = Some(deadline.0.map_or(expiry, |due| due.min(expiry)));
    }

    other.0.clear();
    for request in bridge.drain_inbound() {
        if !request.command.starts_with("wallpaper.") {
            if other.0.len() < 64 {
                other.0.push(request);
            }
            continue;
        }
        let (rc, body) = if request.command == "wallpaper.status" {
            let queues = state.queue_status(&bridge);
            (0, json!({"ready":geometry.0.is_some(),"paused":!preferences.current.enabled || preferences.current.paused || geometry.0.as_ref().is_none_or(|s| s.locked),
                "preferences":preferences.current.tree(),"persistence_error":preferences.error,
                "pointer":{"ready":state.pointer_ready,"valid":pointer.valid()},
                "metrics":{"host_updates":metrics.updates,"update_wall_ns":metrics.update_ns,
                    "suspended_outputs":control.suspended_outputs.len(),
                    "max_update_wall_ns":metrics.max_update_ns,"submitted_frames":metrics.submitted_frames,
                    "failed_frames":metrics.failed_frames,"simulation_steps":state.simulation_steps,
                    "submission_intervals":metrics.submission_intervals,
                    "submission_interval_ns":metrics.submission_interval_ns,
                    "max_submission_interval_ns":metrics.max_submission_interval_ns,
                    "submission_interval_buckets":metrics.submission_interval_buckets,
                    "submission_interval_bucket_ms":[20,40,60,null],
                    "simulation_wall_ns":state.simulation_ns},
                "scene":geometry.0.as_ref().map(|s| json!({"instance":s.instance,"event_seq":s.sequence,"lost_count":s.lost,"obstacles":s.obstacles.len(),"outputs":s.outputs.len(),"covered_outputs":s.covered_outputs,"locked":s.locked})),
                "bus":{"requests":state.requests,"notifications":state.notifications,"resets":state.resets,"bytes":state.bytes,
                    "dropped_messages":state.dropped_messages,"queues":queues}}).to_string())
        } else if request.command.starts_with("wallpaper.props.") {
            preferences.reply(&request)
        } else {
            (10, json!({"error":"unknown_verb"}).to_string())
        };
        let _ = bridge.try_respond(&request, rc, body);
    }
    control.paused = !preferences.current.enabled
        || preferences.current.paused
        || geometry.0.as_ref().is_none_or(|s| s.locked);
    control.fps_limit = Some(preferences.current.fps_limit);
    control.hidden = !preferences.current.enabled;
    if state.generation.is_some()
        && preferences.has_pending_notification()
        && !preferences.publish_pending(&bridge)
    {
        let retry = now + QUERY_INTERVAL;
        deadline.0 = Some(deadline.0.map_or(retry, |due| due.min(retry)));
    }
    state.observe_queues(&bridge);
    if active.is_some_and(|active| !active.0) {
        control.paused = false;
        control.hidden = false;
        control.fps_limit = None;
        control.suspended_outputs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::bus::{BusMessage, BusReply, TestBusPeer, test_bridge};

    fn app() -> (App, TestBusPeer) {
        let (bridge, peer) = test_bridge("wallpaper");
        let mut app = App::new();
        app.insert_resource(bridge)
            .insert_resource(crate::boids::preferences::PreferenceStore::load(None))
            .init_resource::<SceneBus>()
            .init_resource::<SceneGeometry>()
            .init_resource::<ScenePointer>()
            .init_resource::<SceneControl>()
            .init_resource::<SceneMetrics>()
            .init_resource::<SceneUpdateDeadline>()
            .init_resource::<OtherRequests>()
            .add_systems(Update, service);
        (app, peer)
    }
    fn reply(peer: &TestBusPeer, id: u64, body: String) {
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id: id,
            result: Ok(BusReply {
                rc: 0,
                body,
                result: None,
            }),
        });
    }
    fn snapshot(instance: &str, sequence: u64) -> String {
        json!({"info":{"instance":instance},"port":{"event_seq":sequence,"lost_count":0},
            "focus":{"session_lock":"none"},"outputs":{},"surfaces":{}})
        .to_string()
    }
    fn bootstrap(app: &mut App, peer: &TestBusPeer, generation: u64) -> u64 {
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation,
        });
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.props.watch");
        assert!(app.world().resource::<SceneGeometry>().0.is_none());
        reply(
            peer,
            calls[0].request_id,
            json!({"topic":"comp.props.changed","event_seq":5,"lost_count":0}).to_string(),
        );
        app.update();
        assert!(
            app.world().resource::<SceneGeometry>().0.is_none(),
            "watch acknowledgement is not a snapshot"
        );
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.props.get");
        // Geometry-only tests explicitly defer the independent pointer lease.
        app.world_mut().resource_mut::<SceneBus>().pointer_renew =
            Instant::now() + Duration::from_secs(60);
        calls[0].request_id
    }
    #[test]
    fn final_queue_sample_includes_preference_publication() {
        let (mut app, peer) = app();
        {
            let mut state = app.world_mut().resource_mut::<SceneBus>();
            state.generation = Some(1);
            state.next_query = Instant::now() + Duration::from_secs(60);
        }
        app.world_mut()
            .resource_mut::<crate::boids::preferences::PreferenceStore>()
            .invalidate_subscribers();
        app.update();
        assert_eq!(peer.drain_publishes().len(), 1);
        assert_eq!(
            app.world().resource::<SceneBus>().queue_peaks["outbound"],
            1
        );
        assert!(
            !app.world()
                .resource::<crate::boids::preferences::PreferenceStore>()
                .has_pending_notification()
        );
    }

    #[test]
    fn status_keeps_pre_drain_queue_peaks_and_reported_drops() {
        let (mut app, peer) = app();
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Connected,
            generation: 1,
        });
        peer.deliver_event(BusBridgeEvent::DroppedMessages(3));
        for _ in 0..4 {
            peer.deliver_message(BusMessage {
                connection_generation: 1,
                from: "comp".into(),
                command: "comp.props.changed".into(),
                body: "{}".into(),
                headers: BTreeMap::from([("topic".into(), "comp.props.changed".into())]),
            });
        }
        let request = || ctk::bus::InboundRequest {
            connection_generation: 1,
            from: "test-client".into(),
            command: "wallpaper.status".into(),
            headers: BTreeMap::new(),
            body: "{}".into(),
            reply_id: Some("queue-status".into()),
        };
        peer.send(request());
        app.update();
        let responses = peer.drain_responses();
        let status: serde_json::Value = serde_json::from_str(&responses[0].body).unwrap();
        assert_eq!(status["bus"]["dropped_messages"], 3);
        let queues = &status["bus"]["queues"]["channels"];
        assert_eq!(queues["events"]["sampled_peak"], 2);
        assert_eq!(queues["messages"]["sampled_peak"], 4);
        assert_eq!(queues["messages"]["depth"], 0);
        assert_eq!(queues["messages"]["capacity"], 16);
        assert_eq!(queues["inbound"]["sampled_peak"], 1);
        peer.send(request());
        app.update();
        let responses = peer.drain_responses();
        let status: serde_json::Value = serde_json::from_str(&responses[0].body).unwrap();
        assert_eq!(
            status["bus"]["queues"]["channels"]["messages"]["sampled_peak"],
            4
        );
        assert_eq!(status["bus"]["queues"]["channels"]["messages"]["depth"], 0);
        assert_eq!(status["bus"]["dropped_messages"], 3);
    }
    #[test]
    fn quiet_bus_updates_do_not_dirty_preferences_and_reupload_materials() {
        #[derive(Resource, Default)]
        struct Changes(u32);
        let (mut app, peer) = app();
        app.init_resource::<Changes>().add_systems(
            Update,
            (|preferences: Res<crate::boids::preferences::PreferenceStore>,
              mut changes: ResMut<Changes>| {
                if preferences.is_changed() {
                    changes.0 += 1;
                }
            })
            .after(service),
        );
        bootstrap(&mut app, &peer, 1);
        app.world_mut().resource_mut::<Changes>().0 = 0;
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(app.world().resource::<Changes>().0, 0);
    }

    #[test]
    fn native_watch_reports_committed_revision_and_rejects_unknown_path() {
        let (mut app, peer) = app();
        let dir = tempfile::tempdir().unwrap();
        app.insert_resource(crate::boids::preferences::PreferenceStore::load(Some(
            dir.path().join("wallpaper.json"),
        )));
        bootstrap(&mut app, &peer, 1);
        let request = |command: &str, body: serde_json::Value| ctk::bus::InboundRequest {
            connection_generation: 1,
            from: "test-client".into(),
            command: command.into(),
            headers: BTreeMap::from([("broker_origin".into(), "local".into())]),
            body: body.to_string(),
            reply_id: Some("watch-test".into()),
        };
        peer.send(request(
            "wallpaper.props.set",
            json!({"path":"speed","value":90}),
        ));
        peer.send(request("wallpaper.props.watch", json!({"path":"speed"})));
        peer.send(request("wallpaper.props.watch", json!({"path":"missing"})));
        app.update();
        let replies = peer.drain_responses();
        assert_eq!(replies.iter().map(|r| r.rc).collect::<Vec<_>>(), [0, 0, 10]);
        let write: serde_json::Value = serde_json::from_str(&replies[0].body).unwrap();
        let watch: serde_json::Value = serde_json::from_str(&replies[1].body).unwrap();
        assert_eq!(watch["topic"], crate::boids::preferences::CHANGED_TOPIC);
        assert_eq!(watch["event_seq"], 1);
        assert_eq!(watch["instance"], write["instance"]);
        assert_eq!(watch["scope"], "");
    }

    #[test]
    fn native_property_write_pauses_and_survives_store_reload() {
        let (mut app, peer) = app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallpaper.json");
        app.insert_resource(crate::boids::preferences::PreferenceStore::load(Some(
            path.clone(),
        )));
        let id = bootstrap(&mut app, &peer, 1);
        reply(&peer, id, snapshot("comp-one", 6));
        app.update();
        let mut request = ctk::bus::InboundRequest {
            connection_generation: 1,
            from: "test-client".into(),
            command: "wallpaper.props.set".into(),
            headers: BTreeMap::from([("broker_origin".into(), "local".into())]),
            body: json!({"path":"paused","value":true}).to_string(),
            reply_id: Some("property-1".into()),
        };
        peer.send(request.clone());
        let mut status = request.clone();
        status.command = "wallpaper.status".into();
        status.body = "{}".into();
        peer.send(status);
        app.update();
        let responses = peer.drain_responses();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].rc, 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&responses[1].body).unwrap()["paused"],
            true
        );
        assert!(app.world().resource::<SceneControl>().paused);
        assert!(
            crate::boids::preferences::PreferenceStore::load(Some(path))
                .current
                .paused
        );
        request.body = json!({"path":"paused","value":false}).to_string();
        request
            .headers
            .insert("broker_origin".into(), "mesh".into());
        peer.send(request);
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 10);
        assert!(app.world().resource::<SceneControl>().paused);
    }

    #[test]
    fn admitted_mesh_property_write_updates_live_state_and_persists() {
        let (mut app, peer) = app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallpaper.json");
        app.insert_resource(crate::boids::preferences::PreferenceStore::load(Some(
            path.clone(),
        )));
        let id = bootstrap(&mut app, &peer, 1);
        reply(&peer, id, snapshot("comp-one", 6));
        app.update();
        peer.send(ctk::bus::InboundRequest {
            connection_generation: 1,
            from: "bridge-alpha".into(),
            command: "wallpaper.props.set".into(),
            headers: BTreeMap::from([
                ("broker_origin".into(), "mesh".into()),
                ("broker_peer".into(), "alpha".into()),
                ("broker_service".into(), "mix-test".into()),
            ]),
            body: json!({"path":"paused","value":true}).to_string(),
            reply_id: Some("mesh-property".into()),
        });
        app.update();
        assert_eq!(peer.drain_responses()[0].rc, 0);
        assert!(app.world().resource::<SceneControl>().paused);
        assert!(
            crate::boids::preferences::PreferenceStore::load(Some(path))
                .current
                .paused
        );
    }

    #[test]
    fn latest_pointer_burst_is_fenced_and_lock_clears_without_rendering() {
        let (mut app, peer) = app();
        let id = bootstrap(&mut app, &peer, 1);
        let mut scene: serde_json::Value = serde_json::from_str(&snapshot("comp-one", 6)).unwrap();
        scene["outputs"] = json!({"o":{"name":"OUT","x":-800,"y":0,"width":800,"height":600}});
        reply(&peer, id, scene.to_string());
        app.update();
        app.world_mut().resource_mut::<SceneBus>().pointer_ready = true;
        let sample = |generation, sequence| {
            BusMessage {
            connection_generation: generation, from: String::new(), command: "pointer.changed".into(),
            headers: BTreeMap::from([
                ("topic".into(), "comp.pointer.changed".into()),
                ("broker_origin".into(), "local".into()),
                ("broker_service".into(), "comp".into()),
            ]),
            body: json!({"version":1,"instance":"comp-one","event_seq":sequence,
                "timestamp_ms":sequence,"valid":true,"output":"OUT","position":{"x":20.5,"y":30.25}}).to_string(),
        }
        };
        for sequence in 7..1007 {
            peer.deliver_latest_message(sample(1, sequence));
        }
        app.update();
        assert_eq!(
            app.world().resource::<ScenePointer>().for_output("OUT"),
            Some(cosmix_flock::Point::new(20.5, 30.25))
        );
        // An old connection's invalid sample cannot clear the current point.
        let mut stale = sample(0, 1008);
        stale.body = "{}".into();
        peer.deliver_latest_message(stale);
        app.update();
        assert!(app.world().resource::<ScenePointer>().valid());
        // Directed messages can carry a topic string, but cannot forge the
        // owning service stamp. Reject them before they poison the sequence.
        app.world_mut().resource_mut::<ScenePointer>().clear();
        let mut forged = sample(1, u64::MAX);
        forged.headers.remove("broker_service");
        peer.deliver_latest_message(forged);
        app.update();
        assert!(!app.world().resource::<ScenePointer>().valid());
        peer.deliver_latest_message(sample(1, 1008));
        app.update();
        assert!(app.world().resource::<ScenePointer>().valid());
        app.world_mut()
            .resource_mut::<SceneGeometry>()
            .0
            .as_mut()
            .unwrap()
            .locked = true;
        peer.deliver_latest_message(sample(1, 1009));
        app.update();
        assert!(!app.world().resource::<ScenePointer>().valid());
        assert!(app.world().resource::<SceneControl>().paused);
    }

    #[test]
    fn pointer_sample_before_watch_ack_preserves_receipt_time() {
        for expired in [false, true] {
            let (mut app, peer) = app();
            let id = bootstrap(&mut app, &peer, 1);
            let mut scene: serde_json::Value =
                serde_json::from_str(&snapshot("comp-one", 6)).unwrap();
            scene["outputs"] = json!({"o":{"name":"OUT","x":0,"y":0,"width":800,"height":600}});
            reply(&peer, id, scene.to_string());
            app.update();
            {
                let mut state = app.world_mut().resource_mut::<SceneBus>();
                state.pointer_renew = Instant::now();
                state.next_query = Instant::now();
            }
            app.update();
            let call = peer.drain_calls().remove(0);
            assert_eq!(call.command, "comp.pointer.watch");
            peer.deliver_latest_message(BusMessage {
                connection_generation: 1,
                from: String::new(),
                command: "pointer.changed".into(),
                headers: BTreeMap::from([
                    ("topic".into(), "comp.pointer.changed".into()),
                    ("broker_origin".into(), "local".into()),
                    ("broker_service".into(), "comp".into()),
                ]),
                body: json!({"version":1,"instance":"comp-one","event_seq":7,"timestamp_ms":10,
                "valid":true,"output":"OUT","position":{"x":20.5,"y":30.25}})
                .to_string(),
            });
            app.update();
            assert!(!app.world().resource::<ScenePointer>().valid());
            if expired {
                app.world_mut()
                    .resource_mut::<SceneBus>()
                    .pointer_waiting
                    .as_mut()
                    .unwrap()
                    .1 = Instant::now() - crate::boids::pointer::MAX_AGE;
            }
            reply(
                &peer,
                call.request_id,
                json!({"version":1,"topic":"comp.pointer.changed","lease_ms":3000}).to_string(),
            );
            app.update();
            assert_eq!(app.world().resource::<ScenePointer>().valid(), !expired);
        }
    }

    #[test]
    fn pointer_lease_renews_during_dirty_geometry_and_failure_preserves_scene() {
        let (mut app, peer) = app();
        let id = bootstrap(&mut app, &peer, 1);
        reply(&peer, id, snapshot("comp-one", 6));
        app.update();
        {
            let mut state = app.world_mut().resource_mut::<SceneBus>();
            state.pointer_renew = Instant::now();
            state.next_query = Instant::now();
            state.dirty = true;
        }
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].command, "comp.pointer.watch");
        assert!(app.world().resource::<SceneBus>().dirty);
        reply(
            &peer,
            calls[0].request_id,
            json!({"version":1,"topic":"comp.pointer.changed","lease_ms":3000}).to_string(),
        );
        app.update();
        assert!(app.world().resource::<SceneBus>().pointer_ready);
        {
            let mut state = app.world_mut().resource_mut::<SceneBus>();
            state.pointer_renew = Instant::now();
            state.next_query = Instant::now();
        }
        app.update();
        let calls = peer.drain_calls();
        assert_eq!(calls[0].command, "comp.pointer.watch");
        peer.deliver_event(BusBridgeEvent::Reply {
            request_id: calls[0].request_id,
            result: Err("service unavailable".into()),
        });
        app.update();
        assert!(!app.world().resource::<SceneBus>().pointer_ready);
        assert!(app.world().resource::<SceneGeometry>().0.is_some());
        assert!(!app.world().resource::<SceneControl>().paused);
        assert!(app.world().resource::<SceneUpdateDeadline>().0.is_some());
    }
    #[test]
    fn reconnect_rejects_old_reply_and_requires_new_snapshot() {
        let (mut app, peer) = app();
        let old = bootstrap(&mut app, &peer, 1);
        peer.deliver_event(BusBridgeEvent::Connection {
            state: BusConnectionState::Disconnected,
            generation: 1,
        });
        reply(&peer, old, snapshot("old", 6));
        app.update();
        assert!(app.world().resource::<SceneGeometry>().0.is_none());
        assert!(app.world().resource::<SceneControl>().paused);
        let current = bootstrap(&mut app, &peer, 2);
        reply(&peer, old, snapshot("old", 7));
        reply(&peer, current, snapshot("new", 1));
        app.update();
        assert_eq!(
            app.world()
                .resource::<SceneGeometry>()
                .0
                .as_ref()
                .unwrap()
                .instance,
            "new"
        );
        assert!(!app.world().resource::<SceneControl>().paused);
    }
    #[test]
    fn notification_burst_coalesces_and_expiry_pauses_without_frame() {
        let (mut app, peer) = app();
        let id = bootstrap(&mut app, &peer, 1);
        reply(&peer, id, snapshot("comp-one", 6));
        app.update();
        // Fill the test bridge's 16-slot lane exactly; its injection helper is
        // blocking, unlike the production worker's overflow-reporting sender.
        for _ in 0..16 {
            peer.deliver_message(BusMessage {
                connection_generation: 1,
                from: String::new(),
                command: "props.changed".into(),
                body: "{}".into(),
                headers: BTreeMap::from([("topic".into(), "comp.props.changed".into())]),
            });
        }
        app.world_mut().resource_mut::<SceneBus>().next_query = Instant::now();
        app.update();
        assert_eq!(peer.drain_calls().len(), 1);
        app.update();
        assert!(peer.drain_calls().is_empty(), "one snapshot in flight");
        app.world_mut().resource_mut::<SceneBus>().refreshed = Some(Instant::now() - MAX_AGE);
        app.update();
        assert!(app.world().resource::<SceneGeometry>().0.is_none());
        assert!(app.world().resource::<SceneControl>().paused);
    }
    #[test]
    fn overflow_invalidates_and_lower_revision_is_only_valid_for_new_instance() {
        let (mut app, peer) = app();
        let id = bootstrap(&mut app, &peer, 1);
        reply(&peer, id, snapshot("first", 100));
        app.update();
        {
            let mut state = app.world_mut().resource_mut::<SceneBus>();
            state.dirty = true;
            state.next_query = Instant::now();
        }
        app.update();
        let calls = peer.drain_calls();
        reply(&peer, calls[0].request_id, snapshot("replacement", 1));
        app.update();
        assert_eq!(
            app.world()
                .resource::<SceneGeometry>()
                .0
                .as_ref()
                .unwrap()
                .instance,
            "replacement"
        );
        peer.deliver_event(BusBridgeEvent::DroppedMessages(1));
        app.update();
        assert!(app.world().resource::<SceneGeometry>().0.is_none());
        assert!(!app.world().resource::<SceneBus>().watched);
    }
}
