//! Pure battery state and deterministic snapshot differencing.
//!
//! This module has no async runtime, D-Bus, Bus, mesh, or filesystem dependency.

use std::collections::{BTreeMap, BTreeSet};

/// UPower's complete device-state vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl BatteryState {
    /// Map UPower's `UP_DEVICE_STATE_*` integer without collapsing states.
    pub fn from_upower(value: u32) -> Self {
        match value {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Empty => "empty",
            Self::FullyCharged => "fully-charged",
            Self::PendingCharge => "pending-charge",
            Self::PendingDischarge => "pending-discharge",
            Self::Unknown => "unknown",
        }
    }
}

/// UPower's device type, retained as a transport-independent enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Unknown,
    LinePower,
    Battery,
    Ups,
    Peripheral,
}

impl DeviceKind {
    pub fn from_upower(value: u32) -> Self {
        match value {
            1 => Self::LinePower,
            2 => Self::Battery,
            3 => Self::Ups,
            4..=28 => Self::Peripheral,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::LinePower => "line-power",
            Self::Battery => "battery",
            Self::Ups => "ups",
            Self::Peripheral => "peripheral",
        }
    }
}

/// One UPower device or the virtual display battery.
#[derive(Debug, Clone, PartialEq)]
pub struct BatterySnapshot {
    pub id: String,
    pub kind: DeviceKind,
    pub power_supply: bool,
    pub present: bool,
    pub percentage: Option<f64>,
    pub state: BatteryState,
    pub time_to_empty_s: Option<u64>,
    pub time_to_full_s: Option<u64>,
    pub energy_rate_w: Option<f64>,
    /// Remaining full-charge capacity as a percentage of design capacity.
    pub health_percent: Option<f64>,
}

impl BatterySnapshot {
    pub fn system_battery(&self) -> bool {
        self.present
            && self.power_supply
            && matches!(self.kind, DeviceKind::Battery | DeviceKind::Ups)
    }
}

/// Atomic logical view derived from one UPower rescan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PowerSnapshot {
    /// True when the virtual display device says a system power source is present.
    pub present: bool,
    pub on_battery: bool,
    pub display: Option<BatterySnapshot>,
    /// Stable object-path tail to device state, including peripheral batteries.
    pub devices: BTreeMap<String, BatterySnapshot>,
}

impl PowerSnapshot {
    pub fn from_parts(
        on_battery: bool,
        display: Option<BatterySnapshot>,
        devices: BTreeMap<String, BatterySnapshot>,
    ) -> Self {
        let present = display
            .as_ref()
            .is_some_and(BatterySnapshot::system_battery)
            || devices.values().any(BatterySnapshot::system_battery);
        Self {
            present,
            on_battery,
            display,
            devices,
        }
    }

    /// Return a deterministic event sequence transforming `self` into `next`.
    pub fn diff(&self, next: &Self) -> Vec<PowerEvent> {
        let mut events = Vec::new();

        if self.on_battery != next.on_battery {
            events.push(PowerEvent::OnBatteryChanged {
                old: self.on_battery,
                new: next.on_battery,
            });
        }

        if self.display != next.display {
            events.push(PowerEvent::BatteryChanged {
                id: "display".to_string(),
                old: self.display.clone(),
                new: next.display.clone(),
            });
        }

        let ids: BTreeSet<&String> = self.devices.keys().chain(next.devices.keys()).collect();
        for id in ids {
            match (self.devices.get(id), next.devices.get(id)) {
                (None, Some(device)) => events.push(PowerEvent::DeviceAdded {
                    id: id.clone(),
                    device: device.clone(),
                }),
                (Some(device), None) => events.push(PowerEvent::DeviceRemoved {
                    id: id.clone(),
                    device: device.clone(),
                }),
                (Some(old), Some(new)) if old != new => {
                    events.push(PowerEvent::BatteryChanged {
                        id: id.clone(),
                        old: Some(old.clone()),
                        new: Some(new.clone()),
                    });
                }
                _ => {}
            }
        }

        events
    }
}

/// A transport-neutral state edge. Bus topics are an adapter concern.
#[derive(Debug, Clone, PartialEq)]
pub enum PowerEvent {
    BatteryChanged {
        id: String,
        old: Option<BatterySnapshot>,
        new: Option<BatterySnapshot>,
    },
    DeviceAdded {
        id: String,
        device: BatterySnapshot,
    },
    DeviceRemoved {
        id: String,
        device: BatterySnapshot,
    },
    OnBatteryChanged {
        old: bool,
        new: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(id: &str, percentage: f64, state: BatteryState) -> BatterySnapshot {
        BatterySnapshot {
            id: id.into(),
            kind: DeviceKind::Battery,
            power_supply: true,
            present: true,
            percentage: Some(percentage),
            state,
            time_to_empty_s: Some(7_200),
            time_to_full_s: None,
            energy_rate_w: Some(9.5),
            health_percent: Some(91.0),
        }
    }

    fn laptop() -> PowerSnapshot {
        let bat0 = battery("battery_BAT0", 63.0, BatteryState::Discharging);
        PowerSnapshot::from_parts(
            true,
            Some(BatterySnapshot {
                id: "display".into(),
                ..bat0.clone()
            }),
            BTreeMap::from([("battery_BAT0".into(), bat0)]),
        )
    }

    #[test]
    fn laptop_fixture_has_one_present_system_battery() {
        let snapshot = laptop();
        assert!(snapshot.present);
        assert!(snapshot.on_battery);
        assert_eq!(snapshot.devices.len(), 1);
        assert_eq!(snapshot.display.unwrap().health_percent, Some(91.0));
    }

    #[test]
    fn desktop_fixture_has_no_battery() {
        let snapshot = PowerSnapshot::from_parts(false, None, BTreeMap::new());
        assert!(!snapshot.present);
        assert!(!snapshot.on_battery);
        assert!(snapshot.diff(&PowerSnapshot::default()).is_empty());
    }

    #[test]
    fn device_add_then_remove_produces_lifecycle_events() {
        let desktop = PowerSnapshot::default();
        let laptop = laptop();

        let added = desktop.diff(&laptop);
        assert!(added.iter().any(|event| matches!(
            event,
            PowerEvent::DeviceAdded { id, .. } if id == "battery_BAT0"
        )));

        let removed = laptop.diff(&desktop);
        assert!(removed.iter().any(|event| matches!(
            event,
            PowerEvent::DeviceRemoved { id, .. } if id == "battery_BAT0"
        )));
    }

    #[test]
    fn percentage_and_state_transition_is_a_battery_change() {
        let before = laptop();
        let mut after = before.clone();
        let changed = battery("battery_BAT0", 64.5, BatteryState::Charging);
        after.devices.insert("battery_BAT0".into(), changed.clone());
        after.display = Some(BatterySnapshot {
            id: "display".into(),
            ..changed
        });
        after.on_battery = false;

        let events = before.diff(&after);
        assert!(matches!(
            events.first(),
            Some(PowerEvent::OnBatteryChanged {
                old: true,
                new: false
            })
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, PowerEvent::BatteryChanged { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn upower_states_preserve_the_complete_public_vocabulary() {
        assert_eq!(BatteryState::from_upower(0), BatteryState::Unknown);
        assert_eq!(BatteryState::from_upower(1), BatteryState::Charging);
        assert_eq!(BatteryState::from_upower(2), BatteryState::Discharging);
        assert_eq!(BatteryState::from_upower(3), BatteryState::Empty);
        assert_eq!(BatteryState::from_upower(4), BatteryState::FullyCharged);
        assert_eq!(BatteryState::from_upower(5), BatteryState::PendingCharge);
        assert_eq!(BatteryState::from_upower(6), BatteryState::PendingDischarge);
        assert_eq!(BatteryState::from_upower(99), BatteryState::Unknown);
    }
}
