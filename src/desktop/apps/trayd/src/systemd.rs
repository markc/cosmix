//! On-demand systemd unit discovery.

use std::process::Command;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Manager {
    System,
    User,
}

impl Manager {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }

    pub(crate) fn parse(label: &str) -> Option<Self> {
        match label {
            "system" => Some(Self::System),
            "user" => Some(Self::User),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitStatus {
    Active,
    Inactive,
    Failed,
    /// `activating`, `deactivating`, `reloading`, or an ActiveState systemd
    /// adds later. Folding these into `Inactive` labelled a starting daemon
    /// "inactive" and disabled Stop on it — removing the one action that
    /// cancels a hung start. They are a distinct state, not a missing one.
    Transitional,
}

impl UnitStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Transitional => "changing",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonUnit {
    pub(crate) manager: Manager,
    pub(crate) unit: String,
    pub(crate) status: UnitStatus,
}

pub(crate) struct Discovery {
    pub(crate) units: Vec<DaemonUnit>,
    pub(crate) error: String,
}

#[derive(Deserialize)]
struct RawUnit {
    unit: String,
    active: String,
}

pub(crate) fn discover() -> Discovery {
    merge_discoveries(
        discover_manager(Manager::System),
        discover_manager(Manager::User),
    )
}

fn discover_manager(manager: Manager) -> Result<Vec<DaemonUnit>, String> {
    let mut command = discovery_command(manager);
    let output = command.output().map_err(|error| {
        format!(
            "cannot run {} systemctl list-units: {error}",
            manager.label()
        )
    })?;

    if !output.status.success() {
        return Err(command_error(
            &format!("{} systemctl list-units", manager.label()),
            &output,
        ));
    }
    parse_systemctl_units(manager, &output.stdout)
}

fn discovery_command(manager: Manager) -> Command {
    let mut command = Command::new("timeout");
    command.args(["--signal=KILL", "3s", "systemctl"]);
    if manager == Manager::User {
        command.arg("--user");
    }
    command.args([
        "list-units",
        "--type=service",
        "--all",
        "--output=json",
        "cosmix-*",
    ]);
    command
}

fn merge_discoveries(
    system: Result<Vec<DaemonUnit>, String>,
    user: Result<Vec<DaemonUnit>, String>,
) -> Discovery {
    let mut units = Vec::new();
    let mut errors = Vec::new();
    for result in [system, user] {
        match result {
            Ok(mut discovered) => units.append(&mut discovered),
            Err(error) => errors.push(error),
        }
    }
    units.sort_by(|left, right| {
        left.unit
            .cmp(&right.unit)
            .then_with(|| left.manager.cmp(&right.manager))
    });
    Discovery {
        units,
        error: errors.join("; "),
    }
}

pub(crate) fn parse_systemctl_units(
    manager: Manager,
    input: &[u8],
) -> Result<Vec<DaemonUnit>, String> {
    let raw: Vec<RawUnit> = serde_json::from_slice(input)
        .map_err(|error| format!("invalid systemctl JSON: {error}"))?;
    let mut units = raw
        .into_iter()
        .filter(|unit| unit.unit.starts_with("cosmix-") && unit.unit.ends_with(".service"))
        .map(|unit| DaemonUnit {
            manager,
            unit: unit.unit,
            status: match unit.active.as_str() {
                "active" => UnitStatus::Active,
                "failed" => UnitStatus::Failed,
                "inactive" => UnitStatus::Inactive,
                // Anything else is in motion or unknown to us. Reporting it as
                // settled would be a guess in whichever direction we picked.
                _ => UnitStatus::Transitional,
            },
        })
        .collect::<Vec<_>>();
    units.sort_by(|left, right| left.unit.cmp(&right.unit));
    Ok(units)
}

fn command_error(label: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("{label} exited with {}", output.status)
    } else {
        format!("{label} exited with {}: {}", output.status, concise(detail))
    }
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 180;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitional_active_states_are_not_reported_as_settled() {
        // These four were all folded into Inactive, which told the operator a
        // starting daemon was stopped and took Stop away from them.
        for state in ["activating", "deactivating", "reloading", "maintenance"] {
            let fixture = format!(
                r#"[{{"unit":"cosmix-webd.service","load":"loaded","active":"{state}","sub":"start","description":"Web daemon"}}]"#
            );
            let units =
                parse_systemctl_units(Manager::System, fixture.as_bytes()).expect("valid fixture");
            assert_eq!(
                units[0].status,
                UnitStatus::Transitional,
                "{state} must not be reported as a settled state"
            );
            assert_eq!(units[0].status.label(), "changing");
        }

        // The three settled states keep their exact meaning.
        for (state, expected) in [
            ("active", UnitStatus::Active),
            ("inactive", UnitStatus::Inactive),
            ("failed", UnitStatus::Failed),
        ] {
            let fixture = format!(
                r#"[{{"unit":"cosmix-webd.service","load":"loaded","active":"{state}","sub":"x","description":"Web daemon"}}]"#
            );
            let units =
                parse_systemctl_units(Manager::System, fixture.as_bytes()).expect("valid fixture");
            assert_eq!(units[0].status, expected);
        }
    }

    #[test]
    fn parses_and_sorts_systemctl_json_fixture() {
        let fixture = br#"[
            {
                "unit": "cosmix-webd.service",
                "load": "loaded",
                "active": "inactive",
                "sub": "dead",
                "description": "Web daemon"
            },
            {
                "unit": "not-cosmix.service",
                "load": "loaded",
                "active": "active",
                "sub": "running",
                "description": "Ignore me"
            },
            {
                "unit": "cosmix-dnsd.service",
                "load": "loaded",
                "active": "failed",
                "sub": "failed",
                "description": "DNS daemon"
            },
            {
                "unit": "cosmix-noded.service",
                "load": "loaded",
                "active": "active",
                "sub": "running",
                "description": "Node broker"
            }
        ]"#;
        assert_eq!(
            parse_systemctl_units(Manager::System, fixture),
            Ok(vec![
                DaemonUnit {
                    manager: Manager::System,
                    unit: "cosmix-dnsd.service".into(),
                    status: UnitStatus::Failed,
                },
                DaemonUnit {
                    manager: Manager::System,
                    unit: "cosmix-noded.service".into(),
                    status: UnitStatus::Active,
                },
                DaemonUnit {
                    manager: Manager::System,
                    unit: "cosmix-webd.service".into(),
                    status: UnitStatus::Inactive,
                },
            ])
        );
    }

    #[test]
    fn invalid_systemctl_json_is_an_error() {
        assert!(parse_systemctl_units(Manager::System, b"not json").is_err());
    }

    #[test]
    fn discovery_commands_target_both_local_managers() {
        let system = format!("{:?}", discovery_command(Manager::System));
        let user = format!("{:?}", discovery_command(Manager::User));
        assert!(system.contains("systemctl"));
        assert!(!system.contains("\"--user\""));
        assert!(user.contains("systemctl"));
        assert!(user.contains("\"--user\""));
        assert!(!system.contains("ssh"));
        assert!(!user.contains("ssh"));
    }

    #[test]
    fn both_managers_are_merged_and_duplicate_names_remain_distinct() {
        let system = parse_systemctl_units(
            Manager::System,
            br#"[{"unit":"cosmix-shared.service","active":"active"}]"#,
        );
        let user = parse_systemctl_units(
            Manager::User,
            br#"[
                {"unit":"cosmix-musicd.service","active":"active"},
                {"unit":"cosmix-shared.service","active":"inactive"}
            ]"#,
        );
        let discovery = merge_discoveries(system, user);
        assert!(discovery.error.is_empty());
        assert_eq!(
            discovery
                .units
                .iter()
                .map(|unit| (unit.manager.label(), unit.unit.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "cosmix-musicd.service"),
                ("system", "cosmix-shared.service"),
                ("user", "cosmix-shared.service"),
            ]
        );
    }

    #[test]
    fn one_manager_failure_keeps_the_other_managers_units() {
        let system = parse_systemctl_units(
            Manager::System,
            br#"[{"unit":"cosmix-webd.service","active":"active"}]"#,
        );
        let discovery = merge_discoveries(system, Err("user manager unavailable".into()));
        assert_eq!(discovery.units.len(), 1);
        assert_eq!(discovery.units[0].manager, Manager::System);
        assert_eq!(discovery.error, "user manager unavailable");
    }
}
