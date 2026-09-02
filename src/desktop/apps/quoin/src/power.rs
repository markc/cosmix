use ctk::bus::{BusMessage, BusReply};
use serde_json::Value;

#[cfg(test)]
const POWER_REQUEST_BASE: u64 = 0x51_0000_0000;

/// Upper bound on telemetry buffered while a snapshot request is in flight.
/// Crossing it is treated exactly like a delivery gap: the sync restarts with
/// a fresh request instead of growing without bound on a reply that never
/// arrives (`PowerAction::Resync` from the `Syncing` arm).
const MAX_BUFFERED_CHANGES: usize = 256;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PowerProjection {
    pub present: bool,
    pub on_battery: Option<bool>,
    pub battery_present: Option<bool>,
    pub percentage: Option<f64>,
    pub state: Option<String>,
    pub time_to_empty_s: Option<u64>,
    pub time_to_full_s: Option<u64>,
    pub energy_rate_w: Option<f64>,
    pub health_percent: Option<f64>,
}

/// What the caller must do after handing a Bus message to [`PowerSync`].
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PowerAction {
    /// Message consumed (or not power telemetry); nothing to do.
    None,
    /// The projection changed; re-render.
    Changed,
    /// Issue a fresh `power.props.get` keyed on this connection generation.
    Resync { generation: u64 },
}

/// Outcome of applying one `power.props.changed` body to a live projection.
#[derive(Debug, Eq, PartialEq)]
enum ChangeOutcome {
    Applied,
    /// Malformed or irrelevant: no `event_seq`, unparsable body, unknown or
    /// mistyped path. Rendering a fabricated value would violate the
    /// render-honestly law, so the message is dropped without effect.
    Ignored,
    /// `event_seq` at or below the last applied one. Right after a sync this
    /// is legitimate residue (a change published before the snapshot but
    /// delivered after it, on the separate telemetry connection); a steady
    /// stream of it is a restarted powerd republishing from 1.
    Stale,
}

#[derive(Debug, Default)]
pub(crate) enum PowerSync {
    #[default]
    Unavailable,
    Syncing {
        generation: u64,
        request_id: u64,
        buffered: Vec<BusMessage>,
    },
    Ready {
        generation: u64,
        sequence: u64,
        projection: PowerProjection,
    },
}

impl PowerSync {
    #[cfg(test)]
    pub fn reconnect(&mut self, generation: u64) -> u64 {
        let request_id = POWER_REQUEST_BASE.saturating_add(generation);
        self.begin(generation, request_id);
        request_id
    }

    pub fn begin(&mut self, generation: u64, request_id: u64) {
        *self = Self::Syncing {
            generation,
            request_id,
            buffered: Vec::new(),
        };
    }

    pub fn generation(&self) -> Option<u64> {
        match self {
            Self::Syncing { generation, .. } | Self::Ready { generation, .. } => Some(*generation),
            Self::Unavailable => None,
        }
    }

    pub fn invalidate(&mut self) {
        *self = Self::Unavailable;
    }

    /// Route one Bus message. This owns EVERY recovery decision so the Bevy
    /// system stays mechanical glue; in particular:
    ///
    /// - A change (gap or not) arriving while `Unavailable` proves powerd is
    ///   publishing again on a healthy connection, so it begins a new sync
    ///   keyed on the message's own live generation. Without this, a powerd
    ///   that was down when the bridge connected left "Power unavailable"
    ///   permanent until a broker reconnect — which a healthy connection
    ///   never delivers.
    /// - A stale-sequence change while `Ready` is treated as a possible
    ///   publisher restart (powerd's `event_seq` is daemon-session monotonic
    ///   and restarts from 1 with no gap and no reconnect) and triggers a
    ///   re-snapshot. This converges without a timer: every stale message is
    ///   consumed by the resync it triggers (never requeued), post-sync
    ///   residue arriving while `Syncing` is absorbed by the buffered replay's
    ///   sequence gate, so total resyncs are bounded by the count of stale
    ///   messages actually delivered while `Ready`.
    pub fn observe_message(&mut self, message: BusMessage) -> PowerAction {
        if message.topic() != Some("power.props.changed") {
            return PowerAction::None;
        }
        let live = message.connection_generation;
        if message.headers.get("gap").is_some_and(|v| v == "true") {
            return match self.generation() {
                // Stale-epoch gap: the reconnect that obsoleted it already
                // triggered its own sync.
                Some(current) if current != live => PowerAction::None,
                _ => PowerAction::Resync { generation: live },
            };
        }
        match self {
            Self::Unavailable => PowerAction::Resync { generation: live },
            Self::Syncing {
                generation,
                buffered,
                ..
            } if live == *generation => {
                if buffered.len() >= MAX_BUFFERED_CHANGES {
                    return PowerAction::Resync { generation: live };
                }
                buffered.push(message);
                PowerAction::None
            }
            Self::Ready {
                generation,
                sequence,
                projection,
            } if live == *generation => match apply_change(&message, sequence, projection) {
                ChangeOutcome::Applied => PowerAction::Changed,
                ChangeOutcome::Ignored => PowerAction::None,
                ChangeOutcome::Stale => PowerAction::Resync { generation: live },
            },
            _ => PowerAction::None,
        }
    }

    pub fn accept_reply(&mut self, request_id: u64, reply: Result<BusReply, String>) -> bool {
        let Self::Syncing {
            generation,
            request_id: expected,
            buffered,
        } = self
        else {
            return false;
        };
        if request_id != *expected {
            return false;
        }
        let Ok(reply) = reply else {
            self.invalidate();
            return true;
        };
        if reply.rc != 0 {
            self.invalidate();
            return true;
        }
        let Ok(snapshot) = serde_json::from_str::<Value>(&reply.body) else {
            self.invalidate();
            return true;
        };
        // A snapshot without a sequence cannot gate the buffered replay or
        // future changes; one without a boolean `present` cannot be rendered
        // without fabricating "No system battery". Both are contract
        // violations -> honest "Power unavailable", never invented data.
        let Some(sequence) = snapshot
            .pointer("/lifecycle/event_seq")
            .and_then(Value::as_u64)
        else {
            self.invalidate();
            return true;
        };
        let Some(mut projection) = projection_from_snapshot(&snapshot) else {
            self.invalidate();
            return true;
        };
        let generation = *generation;
        let buffered = std::mem::take(buffered);
        let mut current = sequence;
        for message in buffered {
            // Stale entries here are the expected post-snapshot residue; they
            // are dropped by the sequence gate, never treated as a restart.
            let _ = apply_change(&message, &mut current, &mut projection);
        }
        *self = Self::Ready {
            generation,
            sequence: current,
            projection,
        };
        true
    }

    pub fn render(&self) -> String {
        match self {
            Self::Unavailable | Self::Syncing { .. } => "Power unavailable".to_owned(),
            Self::Ready { projection, .. } => render_projection(projection),
        }
    }
}

/// `None` when the snapshot violates the contract (missing or non-boolean
/// `present`): rendering would turn absence into a confident negative claim.
fn projection_from_snapshot(value: &Value) -> Option<PowerProjection> {
    let present = value.get("present").and_then(Value::as_bool)?;
    let battery = value.get("battery");
    Some(PowerProjection {
        present,
        on_battery: value.get("on_battery").and_then(Value::as_bool),
        battery_present: battery
            .and_then(|v| v.get("present"))
            .and_then(Value::as_bool),
        percentage: battery
            .and_then(|v| v.get("percentage"))
            .and_then(Value::as_f64),
        state: battery
            .and_then(|v| v.get("state"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        time_to_empty_s: battery
            .and_then(|v| v.get("time_to_empty_s"))
            .and_then(Value::as_u64),
        time_to_full_s: battery
            .and_then(|v| v.get("time_to_full_s"))
            .and_then(Value::as_u64),
        energy_rate_w: battery
            .and_then(|v| v.get("energy_rate_w"))
            .and_then(Value::as_f64),
        health_percent: battery
            .and_then(|v| v.get("health_percent"))
            .and_then(Value::as_f64),
    })
}

fn apply_change(
    message: &BusMessage,
    sequence: &mut u64,
    projection: &mut PowerProjection,
) -> ChangeOutcome {
    if message.headers.get("gap").is_some_and(|v| v == "true") {
        // Defensive: gaps are intercepted before buffering, but a buffered
        // one must not count as data.
        return ChangeOutcome::Ignored;
    }
    // A change without `event_seq` is a contract violation: it can neither be
    // ordered nor prove a restart. Dropping it (rather than defaulting to 0)
    // keeps a malformed peer from both replaying stale data and triggering a
    // resync storm.
    let Some(event_sequence) = message
        .headers
        .get("event_seq")
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return ChangeOutcome::Ignored;
    };
    if event_sequence <= *sequence {
        return ChangeOutcome::Stale;
    }
    let Ok(body) = serde_json::from_str::<Value>(&message.body) else {
        return ChangeOutcome::Ignored;
    };
    let Some(path) = body.get("path").and_then(Value::as_str) else {
        return ChangeOutcome::Ignored;
    };
    let new = body.get("new").unwrap_or(&Value::Null);
    match path {
        "present" => match new.as_bool() {
            // A missing/mistyped `present` must not fabricate "No system
            // battery" (the boolean twin of rendering a missing value as 0).
            Some(present) => projection.present = present,
            None => return ChangeOutcome::Ignored,
        },
        "on_battery" => projection.on_battery = new.as_bool(),
        "battery.present" => projection.battery_present = new.as_bool(),
        "battery.percentage" => projection.percentage = new.as_f64(),
        "battery.state" => projection.state = new.as_str().map(str::to_owned),
        "battery.time_to_empty_s" => projection.time_to_empty_s = new.as_u64(),
        "battery.time_to_full_s" => projection.time_to_full_s = new.as_u64(),
        "battery.energy_rate_w" => projection.energy_rate_w = new.as_f64(),
        "battery.health_percent" => projection.health_percent = new.as_f64(),
        _ => return ChangeOutcome::Ignored,
    }
    *sequence = event_sequence;
    ChangeOutcome::Applied
}

fn render_projection(power: &PowerProjection) -> String {
    if !power.present || power.battery_present == Some(false) {
        return "No system battery".to_owned();
    }
    let mut fields = vec!["Battery present".to_owned()];
    fields.push(
        power
            .percentage
            .map_or_else(|| "Charge unavailable".to_owned(), |v| format!("{v:.0}%")),
    );
    fields.push(
        power
            .state
            .clone()
            .unwrap_or_else(|| "State unknown".to_owned()),
    );
    if let Some(seconds) = power.time_to_empty_s.or(power.time_to_full_s) {
        fields.push(format!("{}h {:02}m", seconds / 3600, seconds / 60 % 60));
    }
    if let Some(rate) = power.energy_rate_w {
        fields.push(format!("{rate:.1} W"));
    }
    if let Some(health) = power.health_percent {
        fields.push(format!("Health {health:.0}%"));
    }
    fields.join("  •  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn ready(body: &str) -> PowerSync {
        let mut state = PowerSync::Unavailable;
        let id = state.reconnect(7);
        assert!(state.accept_reply(
            id,
            Ok(BusReply {
                rc: 0,
                body: body.to_owned(),
                result: None
            })
        ));
        state
    }

    fn change(generation: u64, event_seq: Option<u64>, body: &str) -> BusMessage {
        let mut headers = BTreeMap::new();
        headers.insert("topic".to_owned(), "power.props.changed".to_owned());
        if let Some(seq) = event_seq {
            headers.insert("event_seq".to_owned(), seq.to_string());
        }
        BusMessage {
            connection_generation: generation,
            from: "power".to_owned(),
            command: "noded.topic.event".to_owned(),
            body: body.to_owned(),
            headers,
        }
    }

    #[test]
    fn power_absent_partial_and_full_render_honestly() {
        assert_eq!(
            ready(r#"{"present":false,"battery":{"present":false},"lifecycle":{"event_seq":1}}"#)
                .render(),
            "No system battery"
        );
        let partial =
            ready(r#"{"present":true,"battery":{"present":true},"lifecycle":{"event_seq":1}}"#)
                .render();
        assert!(partial.contains("Charge unavailable") && partial.contains("State unknown"));
        assert!(!partial.contains("0%"));
        let full = ready(r#"{"present":true,"on_battery":true,"battery":{"present":true,"percentage":73.0,"state":"discharging","time_to_empty_s":7800,"energy_rate_w":12.4,"health_percent":91.0},"lifecycle":{"event_seq":4}}"#).render();
        assert!(
            full.contains("73%")
                && full.contains("discharging")
                && full.contains("2h 10m")
                && full.contains("12.4 W")
                && full.contains("Health 91%")
        );
    }

    #[test]
    fn reconnect_requires_a_new_matching_snapshot() {
        let mut state = ready(
            r#"{"present":true,"battery":{"present":true,"percentage":50.0},"lifecycle":{"event_seq":2}}"#,
        );
        let old_id = POWER_REQUEST_BASE + 7;
        let new_id = state.reconnect(8);
        assert_eq!(state.render(), "Power unavailable");
        assert!(!state.accept_reply(
            old_id,
            Ok(BusReply {
                rc: 0,
                body: "{}".to_owned(),
                result: None
            })
        ));
        assert!(state.accept_reply(new_id, Ok(BusReply { rc: 0, body: r#"{"present":false,"battery":{"present":false},"lifecycle":{"event_seq":9}}"#.to_owned(), result: None })));
        assert_eq!(state.render(), "No system battery");
    }

    /// MAJOR 1 regression: powerd absent at connect (failed snapshot ->
    /// `Unavailable`), then powerd comes back and publishes a change on the
    /// SAME healthy connection — no broker reconnect anywhere. The change
    /// itself must restart the sync and the display must recover.
    #[test]
    fn a_change_while_unavailable_recovers_without_a_reconnect() {
        let mut state = PowerSync::Unavailable;
        let id = state.reconnect(7);
        assert!(state.accept_reply(id, Err("powerd is down".to_owned())));
        assert_eq!(state.render(), "Power unavailable");

        let action = state.observe_message(change(
            7,
            Some(3),
            r#"{"path":"battery.percentage","new":41.0}"#,
        ));
        assert_eq!(action, PowerAction::Resync { generation: 7 });

        let id = state.reconnect(7);
        assert!(state.accept_reply(
            id,
            Ok(BusReply {
                rc: 0,
                body: r#"{"present":true,"battery":{"present":true,"percentage":41.0},"lifecycle":{"event_seq":3}}"#.to_owned(),
                result: None
            })
        ));
        assert!(state.render().contains("41%"));
    }

    /// A gap while `Unavailable` also restarts the sync (same dead-end as
    /// MAJOR 1's non-gap case), while a stale-epoch gap stays inert.
    #[test]
    fn gap_recovery_is_keyed_on_the_live_generation() {
        let mut state = PowerSync::Unavailable;
        let mut gap = change(9, None, "{}");
        gap.headers.insert("gap".to_owned(), "true".to_owned());
        assert_eq!(
            state.observe_message(gap.clone()),
            PowerAction::Resync { generation: 9 }
        );

        let mut ready_state =
            ready(r#"{"present":true,"battery":{"present":true},"lifecycle":{"event_seq":5}}"#);
        // ready() syncs on generation 7; a generation-9 gap is stale residue.
        assert_eq!(ready_state.observe_message(gap), PowerAction::None);
        let mut live_gap = change(7, None, "{}");
        live_gap.headers.insert("gap".to_owned(), "true".to_owned());
        assert_eq!(
            ready_state.observe_message(live_gap),
            PowerAction::Resync { generation: 7 }
        );
    }

    /// MAJOR 2 regression: a restarted powerd republishes `event_seq` from 1
    /// with no gap and no reconnect. The stale sequence must be read as a
    /// possible publisher restart -> re-snapshot, not silently frozen data.
    #[test]
    fn a_stale_sequence_while_ready_resnapshots_and_converges() {
        let mut state = ready(
            r#"{"present":true,"battery":{"present":true,"percentage":80.0},"lifecycle":{"event_seq":40}}"#,
        );
        // powerd restarted: new session, seq restarts at 1.
        assert_eq!(
            state.observe_message(change(
                7,
                Some(1),
                r#"{"path":"battery.percentage","new":35.0}"#
            )),
            PowerAction::Resync { generation: 7 }
        );
        let id = state.reconnect(7);
        assert!(state.accept_reply(
            id,
            Ok(BusReply {
                rc: 0,
                body: r#"{"present":true,"battery":{"present":true,"percentage":35.0},"lifecycle":{"event_seq":1}}"#.to_owned(),
                result: None
            })
        ));
        assert!(state.render().contains("35%"));
        // The new session's next change now applies normally: converged.
        assert_eq!(
            state.observe_message(change(
                7,
                Some(2),
                r#"{"path":"battery.percentage","new":34.0}"#
            )),
            PowerAction::Changed
        );
        assert!(state.render().contains("34%"));
    }

    /// MINOR 5/6 regression: contract-violating input never fabricates data.
    #[test]
    fn malformed_snapshots_and_changes_are_refused_not_rendered() {
        // Snapshot without lifecycle/event_seq.
        let mut state = PowerSync::Unavailable;
        let id = state.reconnect(7);
        assert!(state.accept_reply(
            id,
            Ok(BusReply {
                rc: 0,
                body: r#"{"present":true,"battery":{"present":true}}"#.to_owned(),
                result: None
            })
        ));
        assert_eq!(state.render(), "Power unavailable");

        // Snapshot without a boolean `present` must not claim "No system
        // battery".
        let id = state.reconnect(7);
        assert!(state.accept_reply(
            id,
            Ok(BusReply {
                rc: 0,
                body: r#"{"battery":{"present":true},"lifecycle":{"event_seq":2}}"#.to_owned(),
                result: None
            })
        ));
        assert_eq!(state.render(), "Power unavailable");

        // A change without event_seq is dropped: no data applied AND no
        // resync storm (it must not read as a restart).
        let mut state = ready(
            r#"{"present":true,"battery":{"present":true,"percentage":50.0},"lifecycle":{"event_seq":5}}"#,
        );
        assert_eq!(
            state.observe_message(change(
                7,
                None,
                r#"{"path":"battery.percentage","new":10.0}"#
            )),
            PowerAction::None
        );
        assert!(state.render().contains("50%"));

        // A `present` change with a non-boolean value is dropped, not turned
        // into "No system battery".
        assert_eq!(
            state.observe_message(change(7, Some(6), r#"{"path":"present","new":null}"#)),
            PowerAction::None
        );
        assert!(state.render().contains("50%"));
    }

    /// The Syncing buffer is bounded: overflow restarts the sync like a gap
    /// instead of growing forever on a reply that never arrives.
    #[test]
    fn syncing_buffer_overflow_restarts_the_sync() {
        let mut state = PowerSync::Unavailable;
        let _id = state.reconnect(7);
        for seq in 0..MAX_BUFFERED_CHANGES as u64 {
            assert_eq!(
                state.observe_message(change(
                    7,
                    Some(seq + 1),
                    r#"{"path":"battery.percentage","new":50.0}"#
                )),
                PowerAction::None
            );
        }
        assert_eq!(
            state.observe_message(change(
                7,
                Some(999),
                r#"{"path":"battery.percentage","new":50.0}"#
            )),
            PowerAction::Resync { generation: 7 }
        );
    }
}
