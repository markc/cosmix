use ctk::bus::{BusMessage, BusReply};
use serde_json::Value;

#[cfg(test)]
const POWER_REQUEST_BASE: u64 = 0x51_0000_0000;

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

    pub fn accept_message(&mut self, message: BusMessage) -> bool {
        if message.topic() != Some("power.props.changed") {
            return false;
        }
        match self {
            Self::Syncing {
                generation,
                buffered,
                ..
            } if message.connection_generation == *generation => {
                buffered.push(message);
                false
            }
            Self::Ready {
                generation,
                sequence,
                projection,
            } if message.connection_generation == *generation => {
                apply_change(message, sequence, projection)
            }
            _ => false,
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
        let sequence = snapshot
            .pointer("/lifecycle/event_seq")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut projection = projection_from_snapshot(&snapshot);
        let generation = *generation;
        let buffered = std::mem::take(buffered);
        let mut current = sequence;
        for message in buffered {
            let _ = apply_change(message, &mut current, &mut projection);
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

fn projection_from_snapshot(value: &Value) -> PowerProjection {
    let battery = value.get("battery");
    PowerProjection {
        present: value
            .get("present")
            .and_then(Value::as_bool)
            .unwrap_or(false),
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
    }
}

fn apply_change(message: BusMessage, sequence: &mut u64, projection: &mut PowerProjection) -> bool {
    if message.headers.get("gap").is_some_and(|v| v == "true") {
        return false;
    }
    let event_sequence = message
        .headers
        .get("event_seq")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if event_sequence <= *sequence {
        return false;
    }
    let Ok(body) = serde_json::from_str::<Value>(&message.body) else {
        return false;
    };
    let Some(path) = body.get("path").and_then(Value::as_str) else {
        return false;
    };
    let new = body.get("new").unwrap_or(&Value::Null);
    match path {
        "present" => projection.present = new.as_bool().unwrap_or(false),
        "on_battery" => projection.on_battery = new.as_bool(),
        "battery.present" => projection.battery_present = new.as_bool(),
        "battery.percentage" => projection.percentage = new.as_f64(),
        "battery.state" => projection.state = new.as_str().map(str::to_owned),
        "battery.time_to_empty_s" => projection.time_to_empty_s = new.as_u64(),
        "battery.time_to_full_s" => projection.time_to_full_s = new.as_u64(),
        "battery.energy_rate_w" => projection.energy_rate_w = new.as_f64(),
        "battery.health_percent" => projection.health_percent = new.as_f64(),
        _ => return false,
    }
    *sequence = event_sequence;
    true
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
}
