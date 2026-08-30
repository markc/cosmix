//! Bounded, event-driven model for Tower's broker traffic pane.

use std::collections::VecDeque;

use bevy::ecs::message::Message;
use bevy::prelude::Resource;
use ctk::prelude::BusMessage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MAX_TRAFFIC_ROWS: usize = 2048;
pub(crate) const MAX_PAUSED_ROWS: usize = 2048;
pub(crate) const MAX_RENDERED_ROWS: usize = 128;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrafficDirection {
    #[default]
    All,
    Local,
    MeshIn,
    MeshOut,
}

impl TrafficDirection {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Local => "local",
            Self::MeshIn => "mesh in",
            Self::MeshOut => "mesh out",
        }
    }

    fn wire(self) -> Vec<&'static str> {
        match self {
            Self::All => vec!["local", "mesh_in", "mesh_out"],
            Self::Local => vec!["local"],
            Self::MeshIn => vec!["mesh_in"],
            Self::MeshOut => vec!["mesh_out"],
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Local,
            Self::Local => Self::MeshIn,
            Self::MeshIn => Self::MeshOut,
            Self::MeshOut => Self::All,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrafficBody {
    #[default]
    None,
    Redacted,
}

impl TrafficBody {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::None => "off",
            Self::Redacted => "redacted",
        }
    }

    const fn wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Redacted => "redacted",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(default)]
pub(crate) struct TrafficFilter {
    pub verb_glob: String,
    pub service: Option<String>,
    pub direction: TrafficDirection,
    pub body: TrafficBody,
}

impl Default for TrafficFilter {
    fn default() -> Self {
        Self {
            verb_glob: "*".into(),
            service: None,
            direction: TrafficDirection::All,
            body: TrafficBody::None,
        }
    }
}

impl TrafficFilter {
    pub(crate) fn start_body(&self) -> String {
        let services = self
            .service
            .as_ref()
            .map_or_else(Vec::new, |service| vec![service.as_str()]);
        json!({
            "filter": {
                "verbs": [self.verb_glob],
                "services": services,
                "directions": self.direction.wire(),
            },
            "body": self.body.wire(),
            "capacity": 1024,
        })
        .to_string()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct TrafficEvent {
    pub seq: u64,
    pub ts: String,
    pub direction: String,
    pub outcome: String,
    pub message_type: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub verb: Option<String>,
    pub size: usize,
    pub correlation_id: Option<String>,
    pub rc: Option<i64>,
    #[serde(default)]
    pub dropped_count: u64,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub payload_omitted: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ObserveStartReply {
    pub subscription_id: String,
}

#[derive(Resource, Clone, Debug)]
pub(crate) struct TrafficState {
    pub open: bool,
    pub paused: bool,
    pub filter: TrafficFilter,
    pub subscription_id: Option<String>,
    pub status: String,
    pub rows: VecDeque<TrafficEvent>,
    paused_rows: VecDeque<TrafficEvent>,
    pub selected_seq: Option<u64>,
    pub server_dropped: u64,
    pub client_dropped: u64,
    pub observation_connection: ctk::prelude::BusConnectionState,
    pub connection_generation: u64,
    pub filter_revision: u64,
    pub revision: u64,
}

impl Default for TrafficState {
    fn default() -> Self {
        Self {
            open: false,
            paused: false,
            filter: TrafficFilter::default(),
            subscription_id: None,
            status: "Traffic pane closed".into(),
            rows: VecDeque::new(),
            paused_rows: VecDeque::new(),
            selected_seq: None,
            server_dropped: 0,
            client_dropped: 0,
            observation_connection: ctk::prelude::BusConnectionState::Connecting,
            connection_generation: 0,
            filter_revision: 1,
            revision: 1,
        }
    }
}

impl TrafficState {
    pub(crate) fn with_filter(filter: TrafficFilter) -> Self {
        Self {
            filter,
            ..Self::default()
        }
    }

    pub(crate) fn selected(&self) -> Option<&TrafficEvent> {
        let seq = self.selected_seq?;
        self.rows
            .iter()
            .chain(self.paused_rows.iter())
            .find(|event| event.seq == seq)
    }

    pub(crate) fn paused_len(&self) -> usize {
        self.paused_rows.len()
    }

    pub(crate) fn connected(&mut self, generation: u64) -> bool {
        let fresh = self.connection_generation != generation;
        if fresh {
            self.connection_generation = generation;
            self.subscription_id = None;
            self.status = if self.open {
                "Resubscribing to live traffic...".into()
            } else {
                "Traffic pane closed".into()
            };
            self.bump();
        }
        fresh && self.open
    }

    pub(crate) fn disconnected(&mut self) {
        let had_subscription = self.subscription_id.is_some();
        self.subscription_id = None;
        self.status = if self.open {
            "Traffic unavailable until the dedicated observation connection reconnects".into()
        } else if had_subscription {
            "Stop pending: observation wire is down; broker disconnect cleanup is the backstop"
                .into()
        } else {
            "Traffic pane closed".into()
        };
        self.bump();
    }

    pub(crate) fn start_succeeded(&mut self, body: &str) -> Result<(), String> {
        let reply: ObserveStartReply = serde_json::from_str(body)
            .map_err(|error| format!("invalid noded.observe.start reply: {error}"))?;
        self.rows.clear();
        self.paused_rows.clear();
        self.selected_seq = None;
        self.subscription_id = Some(reply.subscription_id);
        self.status = "Observing live broker traffic".into();
        self.bump();
        Ok(())
    }

    pub(crate) fn stop_succeeded(&mut self) {
        self.subscription_id = None;
        self.status = if self.open {
            "Updating traffic subscription...".into()
        } else {
            "Traffic pane closed".into()
        };
        self.bump();
    }

    pub(crate) fn request_failed(&mut self, error: String) {
        self.subscription_id = None;
        self.status = format!("Traffic subscription failed: {error}");
        self.bump();
    }

    pub(crate) fn record_transport_drops(&mut self, count: usize) {
        self.client_dropped = self.client_dropped.saturating_add(count as u64);
        self.status = format!("CTK traffic queue dropped {count} messages");
        self.bump();
    }

    pub(crate) fn stop_failed(&mut self, error: String) {
        self.status = format!("Traffic stop failed; subscription retained: {error}");
        self.bump();
    }

    pub(crate) fn stop_pending(&mut self, disconnected: bool) {
        self.status = if disconnected {
            "Stop pending: observation wire is down; broker disconnect cleanup is the backstop"
                .into()
        } else {
            "Stop pending on the dedicated observation lane...".into()
        };
        self.bump();
    }

    pub(crate) fn request_queue_busy(&mut self, error: String) {
        self.status = if self.open {
            format!("Traffic subscription pending: {error}")
        } else {
            format!("Stop pending on the dedicated observation lane: {error}")
        };
        self.bump();
    }

    pub(crate) fn handle_message(&mut self, message: &BusMessage) -> Option<TrafficEvent> {
        if message.command != "noded.observe.event" {
            return None;
        }
        if message.connection_generation != self.connection_generation {
            return None;
        }
        let expected = self.subscription_id.as_deref()?;
        if message.headers.get("subscription_id").map(String::as_str) != Some(expected) {
            return None;
        }
        let Ok(event) = serde_json::from_str::<TrafficEvent>(&message.body) else {
            self.status = "Ignored malformed observation event".into();
            self.bump();
            return None;
        };
        self.server_dropped = self.server_dropped.saturating_add(event.dropped_count);
        if self.paused {
            push_bounded(
                &mut self.paused_rows,
                event.clone(),
                MAX_PAUSED_ROWS,
                &mut self.client_dropped,
            );
        } else {
            push_bounded(
                &mut self.rows,
                event.clone(),
                MAX_TRAFFIC_ROWS,
                &mut self.client_dropped,
            );
        }
        self.bump();
        Some(event)
    }

    pub(crate) fn set_open(&mut self, open: bool) {
        if self.open != open {
            self.open = open;
            self.status = if open {
                "Opening live traffic subscription...".into()
            } else {
                "Closing live traffic subscription...".into()
            };
            self.bump();
        }
    }

    pub(crate) fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            while let Some(event) = self.paused_rows.pop_front() {
                push_bounded(
                    &mut self.rows,
                    event,
                    MAX_TRAFFIC_ROWS,
                    &mut self.client_dropped,
                );
            }
        }
        self.bump();
    }

    pub(crate) fn cycle_verb(&mut self) {
        self.filter.verb_glob = match self.filter.verb_glob.as_str() {
            "*" => "noded.*",
            "noded.*" => "topic.*",
            "topic.*" => "*.props.*",
            _ => "*",
        }
        .into();
        self.filter_changed();
    }

    pub(crate) fn cycle_service(&mut self, services: &[String]) {
        let mut choices: Vec<Option<String>> = vec![None, Some("noded".into())];
        choices.extend(
            services
                .iter()
                .filter(|service| service.as_str() != "noded")
                .cloned()
                .map(Some),
        );
        choices.dedup();
        let current = choices
            .iter()
            .position(|choice| choice == &self.filter.service)
            .unwrap_or(0);
        self.filter.service = choices[(current + 1) % choices.len()].clone();
        self.filter_changed();
    }

    pub(crate) fn cycle_direction(&mut self) {
        self.filter.direction = self.filter.direction.next();
        self.filter_changed();
    }

    pub(crate) fn toggle_body(&mut self) {
        self.filter.body = match self.filter.body {
            TrafficBody::None => TrafficBody::Redacted,
            TrafficBody::Redacted => TrafficBody::None,
        };
        self.filter_changed();
    }

    pub(crate) fn apply_filter(&mut self, filter: TrafficFilter) {
        if self.filter != filter {
            self.filter = filter;
            self.filter_changed();
        }
    }

    pub(crate) fn select(&mut self, seq: u64) {
        self.selected_seq = Some(seq);
        self.bump();
    }

    pub(crate) fn notice(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.bump();
    }

    fn filter_changed(&mut self) {
        self.filter_revision = self.filter_revision.wrapping_add(1);
        self.status = "Updating traffic subscription...".into();
        self.bump();
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

fn push_bounded(
    rows: &mut VecDeque<TrafficEvent>,
    event: TrafficEvent,
    limit: usize,
    dropped: &mut u64,
) {
    if rows.len() == limit {
        rows.pop_front();
        *dropped = dropped.saturating_add(1);
    }
    rows.push_back(event);
}

#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrafficIntent {
    SetOpen(bool),
    TogglePause,
    CycleVerb,
    CycleService,
    CycleDirection,
    ToggleBody,
    SaveNamed(String),
    SelectNamed(String),
    DeleteNamed(String),
    Select(u64),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn event(seq: u64, dropped: u64) -> TrafficEvent {
        TrafficEvent {
            seq,
            ts: "2026-07-24T12:00:00Z".into(),
            direction: "local".into(),
            outcome: "delivered".into(),
            message_type: "request".into(),
            from: Some("tower-bevy-1".into()),
            to: Some("noded".into()),
            verb: Some("noded.list".into()),
            size: 90,
            correlation_id: Some("tower-1".into()),
            rc: None,
            dropped_count: dropped,
            payload: None,
            payload_omitted: None,
        }
    }

    fn message(subscription_id: &str, event: &TrafficEvent) -> BusMessage {
        BusMessage {
            connection_generation: 1,
            from: "noded".into(),
            command: "noded.observe.event".into(),
            body: serde_json::to_string(event).unwrap(),
            headers: BTreeMap::from([("subscription_id".into(), subscription_id.into())]),
        }
    }

    #[test]
    fn parses_events_and_counts_broker_drops() {
        let mut state = TrafficState {
            subscription_id: Some("obs-1".into()),
            connection_generation: 1,
            ..TrafficState::default()
        };
        assert!(state
            .handle_message(&message("obs-1", &event(4, 3)))
            .is_some());
        assert_eq!(
            state.rows.front().unwrap().correlation_id.as_deref(),
            Some("tower-1")
        );
        assert_eq!(state.server_dropped, 3);
    }

    #[test]
    fn scrollback_and_pause_buffers_are_bounded() {
        let mut state = TrafficState {
            subscription_id: Some("obs-1".into()),
            connection_generation: 1,
            paused: true,
            ..TrafficState::default()
        };
        for seq in 0..(MAX_PAUSED_ROWS as u64 + 5) {
            state.handle_message(&message("obs-1", &event(seq, 0)));
        }
        assert_eq!(state.paused_len(), MAX_PAUSED_ROWS);
        assert_eq!(state.client_dropped, 5);
        state.toggle_pause();
        assert_eq!(state.paused_len(), 0);
        assert_eq!(state.rows.len(), MAX_TRAFFIC_ROWS);
    }

    #[test]
    fn filter_controls_round_trip_to_the_observe_contract() {
        let filter = TrafficFilter {
            verb_glob: "maild.*".into(),
            service: Some("maild".into()),
            direction: TrafficDirection::MeshIn,
            body: TrafficBody::Redacted,
        };
        let body: Value = serde_json::from_str(&filter.start_body()).unwrap();
        assert_eq!(body["filter"]["verbs"], json!(["maild.*"]));
        assert_eq!(body["filter"]["services"], json!(["maild"]));
        assert_eq!(body["filter"]["directions"], json!(["mesh_in"]));
        assert_eq!(body["body"], "redacted");
        assert_eq!(body["capacity"], 1024);
    }

    #[test]
    fn reconnect_requires_a_fresh_subscription_without_resume() {
        let mut state = TrafficState {
            open: true,
            subscription_id: Some("old".into()),
            connection_generation: 4,
            ..TrafficState::default()
        };
        assert!(state.connected(5));
        assert_eq!(state.subscription_id, None);
        assert!(!state.connected(5));
    }
}
