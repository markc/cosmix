//! Read-only SPEC-07/SPEC-12 property projection for `power.props.*`.

use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

use crate::core::{BatterySnapshot, PowerSnapshot};

/// Owned projection so no daemon lock is held while props-core dispatches.
pub struct PowerProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl PowerProps {
    pub fn new(snapshot: &PowerSnapshot, event_seq: u64, publisher_loss: u64) -> Self {
        let mut leaves = Vec::with_capacity(16 + snapshot.devices.len() * 10);
        push(&mut leaves, "lifecycle.props_level", "L2".into());
        push(&mut leaves, "lifecycle.event_seq", event_seq.into());
        push(
            &mut leaves,
            "lifecycle.publisher_loss",
            publisher_loss.into(),
        );
        push(&mut leaves, "present", snapshot.present.into());
        push(&mut leaves, "on_battery", snapshot.on_battery.into());
        if let Some(display) = &snapshot.display {
            push_battery(&mut leaves, "battery", display);
        } else {
            push(&mut leaves, "battery.present", snapshot.present.into());
        }
        for (id, device) in &snapshot.devices {
            push_battery(&mut leaves, &format!("devices.{id}"), device);
        }
        Self { leaves }
    }
}

impl PropTree for PowerProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        if !self.leaves.iter().any(|(candidate, _)| candidate == path) {
            return None;
        }
        let leaf = path.as_str().rsplit('.').next()?;
        let mut description = match leaf {
            "props_level" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "SPEC-07 event conformance level.",
            ),
            "event_seq" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Monotonic event sequence for this daemon process.",
            ),
            "publisher_loss" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Cumulative publications lost during this daemon process.",
            ),
            "present" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether the battery or power source is currently present.",
            ),
            "on_battery" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether UPower reports the system is running on battery.",
            ),
            "kind" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "UPower device class reduced to a stable public vocabulary.",
            ),
            "power_supply" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether the device supplies power to the system.",
            ),
            "percentage" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Reported remaining charge percentage.",
            )
            .with_min(0.0)
            .with_max(100.0)
            .with_unit("percent"),
            "state" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "UPower state: unknown, charging, discharging, empty, fully-charged, pending-charge, or pending-discharge.",
            ),
            "time_to_empty_s" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Estimated seconds until empty; absent when UPower reports unknown.",
            )
            .with_unit("seconds"),
            "time_to_full_s" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Estimated seconds until full; absent when UPower reports unknown.",
            )
            .with_unit("seconds"),
            "energy_rate_w" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Energy flow rate; positive is discharging and negative is charging.",
            )
            .with_unit("watts"),
            "health_percent" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Full-charge capacity as a percentage of design capacity.",
            )
            .with_min(0.0)
            .with_max(100.0)
            .with_unit("percent"),
            _ => return None,
        };
        // SPEC-07 transient means excluded from props.changed, not merely
        // volatile. Only the self-referential sequence watermark is excluded;
        // every power-state leaf participates in the L2 event stream.
        description.transient = matches!(leaf, "event_seq" | "publisher_loss");
        Some(description)
    }
}

fn push_battery(leaves: &mut Vec<(PropPath, PropValue)>, base: &str, battery: &BatterySnapshot) {
    push(
        leaves,
        &format!("{base}.kind"),
        battery.kind.as_str().into(),
    );
    push(
        leaves,
        &format!("{base}.power_supply"),
        battery.power_supply.into(),
    );
    push(leaves, &format!("{base}.present"), battery.present.into());
    if let Some(value) = battery.percentage {
        push(leaves, &format!("{base}.percentage"), value.into());
    }
    push(
        leaves,
        &format!("{base}.state"),
        battery.state.as_str().into(),
    );
    if let Some(value) = battery.time_to_empty_s {
        push(leaves, &format!("{base}.time_to_empty_s"), value.into());
    }
    if let Some(value) = battery.time_to_full_s {
        push(leaves, &format!("{base}.time_to_full_s"), value.into());
    }
    if let Some(value) = battery.energy_rate_w {
        push(leaves, &format!("{base}.energy_rate_w"), value.into());
    }
    if let Some(value) = battery.health_percent {
        push(leaves, &format!("{base}.health_percent"), value.into());
    }
}

fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    if let Ok(path) = PropPath::new(path) {
        leaves.push((path, value));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::*;
    use crate::core::{BatteryState, DeviceKind};

    fn fixture() -> PowerSnapshot {
        let device = BatterySnapshot {
            id: "battery_bat0".into(),
            kind: DeviceKind::Battery,
            power_supply: true,
            present: true,
            percentage: Some(42.5),
            state: BatteryState::Discharging,
            time_to_empty_s: Some(3_600),
            time_to_full_s: None,
            energy_rate_w: Some(12.0),
            health_percent: Some(89.0),
        };
        PowerSnapshot::from_parts(
            true,
            Some(BatterySnapshot {
                id: "display".into(),
                ..device.clone()
            }),
            BTreeMap::from([("battery_bat0".into(), device)]),
        )
    }

    #[test]
    fn snapshot_uses_stable_battery_and_device_paths() {
        let props = PowerProps::new(&fixture(), 9, 3);
        let snapshot: Value = (&props.snapshot()).into();
        assert_eq!(snapshot["present"], true);
        assert_eq!(snapshot["on_battery"], true);
        assert_eq!(snapshot["battery"]["percentage"], 42.5);
        assert_eq!(snapshot["battery"]["state"], "discharging");
        assert_eq!(snapshot["devices"]["battery_bat0"]["health_percent"], 89.0);
        assert_eq!(snapshot["lifecycle"]["event_seq"], 9);
        assert_eq!(snapshot["lifecycle"]["publisher_loss"], 3);
    }

    #[test]
    fn absent_fixture_still_exposes_present_false() {
        let props = PowerProps::new(&PowerSnapshot::default(), 0, 0);
        let snapshot: Value = (&props.snapshot()).into();
        assert_eq!(snapshot["present"], false);
        assert_eq!(snapshot["battery"]["present"], false);
        assert!(snapshot["battery"].get("percentage").is_none());
    }

    #[test]
    fn power_state_is_event_bearing_but_sequence_watermark_is_transient() {
        let props = PowerProps::new(&fixture(), 9, 2);
        let state = PropPath::new("battery.state").unwrap();
        let sequence = PropPath::new("lifecycle.event_seq").unwrap();
        let publisher_loss = PropPath::new("lifecycle.publisher_loss").unwrap();
        assert!(!props.describe(&state).unwrap().transient);
        assert!(props.describe(&sequence).unwrap().transient);
        assert!(props.describe(&publisher_loss).unwrap().transient);
    }
}
