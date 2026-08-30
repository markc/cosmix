//! Strict, shared interpretation of signed inventory membership for routing.
//!
//! Both noded and wgd run this after cryptographic/fold verification. The
//! entire view rejects if any member is malformed: silently dropping a bad
//! record would let consumers assign different membership meaning to one
//! signed epoch. Tombstones are the exception only in what fields they own: a
//! retired name has no route, so `bus` and `mesh_ip` are neither required nor
//! interpreted for `status:"tombstoned"`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use cosmix_bus::bus::is_valid_label;

/// Fleet-wide default broker port for signed routing members that predate the
/// optional `noded_port` field.
pub const DEFAULT_NODED_PORT: u16 = 4200;

/// One member's strict D1.4 routing classification. A malformed entry is
/// retained in [`RoutingViewError`] so rejection identifies the offending
/// record; a successful view contains only the first three variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingMember {
    ActiveBus {
        name: String,
        mesh_ip: IpAddr,
        noded_port: u16,
    },
    Tombstoned {
        name: String,
    },
    TransportOnly {
        name: String,
        mesh_ip: IpAddr,
    },
    Malformed {
        name: Option<String>,
        reason: String,
    },
}

impl RoutingMember {
    /// Stable JSON representation used by routing-view reports.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::ActiveBus {
                name,
                mesh_ip,
                noded_port,
            } => serde_json::json!({
                "class": "active-bus",
                "name": name,
                "mesh_ip": mesh_ip,
                "noded_port": noded_port,
            }),
            Self::Tombstoned { name } => serde_json::json!({
                "class": "tombstoned",
                "name": name,
            }),
            Self::TransportOnly { name, mesh_ip } => serde_json::json!({
                "class": "transport-only",
                "name": name,
                "mesh_ip": mesh_ip,
            }),
            Self::Malformed { name, reason } => serde_json::json!({
                "class": "malformed",
                "name": name,
                "reason": reason,
            }),
        }
    }
}

/// Why a cryptographically valid inventory has no safe shared routing view.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingViewError {
    #[error("inventory subnet {subnet:?} is not a valid network CIDR: {reason}")]
    BadSubnet { subnet: String, reason: String },
    #[error("{reason}")]
    MalformedMember {
        member: RoutingMember,
        reason: String,
    },
    #[error("member set contains zero active bus routing members")]
    ZeroActiveBus,
}

#[derive(Debug, Clone, Copy)]
struct Subnet {
    network: IpAddr,
    prefix: u8,
}

impl Subnet {
    fn parse(text: &str) -> Result<Self, String> {
        let (address, prefix) = text
            .split_once('/')
            .filter(|(_, prefix)| !prefix.contains('/'))
            .ok_or_else(|| "expected address/prefix".to_string())?;
        let network: IpAddr = address
            .parse()
            .map_err(|_| format!("invalid network address {address:?}"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("invalid prefix length {prefix:?}"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("prefix length {prefix} exceeds {max}"));
        }
        let subnet = Self { network, prefix };
        if !subnet.contains(&network) || subnet.masked(network) != network {
            return Err("network address has host bits set".into());
        }
        Ok(subnet)
    }

    fn masked(self, ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(ip) => {
                let bits = u32::from(ip);
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - u32::from(self.prefix))
                };
                IpAddr::V4((bits & mask).into())
            }
            IpAddr::V6(ip) => {
                let bits = u128::from(ip);
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - u32::from(self.prefix))
                };
                IpAddr::V6((bits & mask).into())
            }
        }
    }

    fn contains(self, ip: &IpAddr) -> bool {
        std::mem::discriminant(&self.network) == std::mem::discriminant(ip)
            && self.masked(*ip) == self.network
    }
}

/// Derive the complete signed membership routing view.
///
/// `subnet` is part of the signed payload. Every active member must carry a
/// parseable `mesh_ip` inside it and an explicit boolean `bus`; tombstones need
/// only a unique SPEC 01 §4.1 label `name` and the known tombstoned status.
pub fn strict_routing_view(
    members: &serde_json::Value,
    subnet: &str,
) -> Result<Vec<RoutingMember>, RoutingViewError> {
    let subnet_cidr = Subnet::parse(subnet).map_err(|reason| RoutingViewError::BadSubnet {
        subnet: subnet.to_string(),
        reason,
    })?;
    let Some(records) = members.as_array() else {
        return Err(malformed(None, "members is not an array".into()));
    };
    let mut view = Vec::with_capacity(records.len());
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut active_ips: BTreeMap<IpAddr, String> = BTreeMap::new();

    for (index, record) in records.iter().enumerate() {
        let name = record
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let reject = |reason: String| malformed(name.clone(), format!("member[{index}]: {reason}"));
        let Some(name_value) = name.as_ref() else {
            return Err(reject("missing string name".into()));
        };
        if !is_valid_label(name_value) {
            return Err(reject(format!(
                "invalid bus label {name_value:?} (SPEC 01 §4.1)"
            )));
        }
        if !names.insert(name_value.clone()) {
            return Err(reject(format!("duplicate name {name_value:?}")));
        }
        let Some(status) = record.get("status").and_then(serde_json::Value::as_str) else {
            return Err(reject("missing string status".into()));
        };

        match status {
            "tombstoned" => {
                view.push(RoutingMember::Tombstoned {
                    name: name_value.clone(),
                });
                continue;
            }
            "active" => {}
            other => return Err(reject(format!("unknown status {other:?}"))),
        }

        let Some(bus) = record.get("bus").and_then(serde_json::Value::as_bool) else {
            return Err(reject("missing boolean bus".into()));
        };
        let Some(mesh_ip_text) = record.get("mesh_ip").and_then(serde_json::Value::as_str) else {
            return Err(reject("missing string mesh_ip".into()));
        };
        let mesh_ip = mesh_ip_text
            .parse::<IpAddr>()
            .map_err(|_| reject(format!("invalid mesh_ip {mesh_ip_text:?}")))?;
        if !subnet_cidr.contains(&mesh_ip) {
            return Err(reject(format!(
                "mesh_ip {mesh_ip} is outside inventory subnet {subnet:?}"
            )));
        }
        if let Some(first) = active_ips.insert(mesh_ip, name_value.clone()) {
            return Err(reject(format!(
                "duplicate active mesh_ip {mesh_ip} (first used by {first:?})"
            )));
        }

        if bus {
            let noded_port = match record.get("noded_port") {
                None => DEFAULT_NODED_PORT,
                Some(value) => value
                    .as_u64()
                    .filter(|port| (1..=u64::from(u16::MAX)).contains(port))
                    .map(|port| port as u16)
                    .ok_or_else(|| {
                        reject("noded_port must be a JSON integer in the range 1..=65535".into())
                    })?,
            };
            view.push(RoutingMember::ActiveBus {
                name: name_value.clone(),
                mesh_ip,
                noded_port,
            });
        } else {
            view.push(RoutingMember::TransportOnly {
                name: name_value.clone(),
                mesh_ip,
            });
        }
    }

    if !view
        .iter()
        .any(|entry| matches!(entry, RoutingMember::ActiveBus { .. }))
    {
        return Err(RoutingViewError::ZeroActiveBus);
    }
    Ok(view)
}

fn malformed(name: Option<String>, reason: String) -> RoutingViewError {
    RoutingViewError::MalformedMember {
        member: RoutingMember::Malformed {
            name,
            reason: reason.clone(),
        },
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUBNET: &str = "192.0.2.0/24";

    #[test]
    fn classifies_active_transport_and_route_free_tombstone() {
        let view = strict_routing_view(
            &json!([
                { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                { "name": "beta", "mesh_ip": "192.0.2.6", "bus": false, "status": "active" },
                { "name": "retired", "status": "tombstoned" }
            ]),
            SUBNET,
        )
        .expect("valid routing view");
        assert_eq!(
            view,
            vec![
                RoutingMember::ActiveBus {
                    name: "alpha".into(),
                    mesh_ip: "192.0.2.5".parse().unwrap(),
                    noded_port: DEFAULT_NODED_PORT,
                },
                RoutingMember::TransportOnly {
                    name: "beta".into(),
                    mesh_ip: "192.0.2.6".parse().unwrap(),
                },
                RoutingMember::Tombstoned {
                    name: "retired".into(),
                },
            ]
        );
    }

    #[test]
    fn rejects_each_malformed_member_class() {
        let active = json!({
            "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active"
        });
        let cases = [
            (
                "duplicate-name",
                json!([active.clone(), active.clone()]),
                "member[1]: duplicate name \"alpha\"",
            ),
            (
                "duplicate-ip",
                json!([
                    active.clone(),
                    { "name": "beta", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" }
                ]),
                "member[1]: duplicate active mesh_ip 192.0.2.5",
            ),
            (
                "bad-ip",
                json!([
                    active.clone(),
                    { "name": "beta", "mesh_ip": "not-an-ip", "bus": true, "status": "active" }
                ]),
                "member[1]: invalid mesh_ip \"not-an-ip\"",
            ),
            (
                "missing-status",
                json!([
                    active.clone(),
                    { "name": "beta", "mesh_ip": "192.0.2.6", "bus": true }
                ]),
                "member[1]: missing string status",
            ),
            (
                "unknown-status",
                json!([
                    active,
                    { "name": "beta", "mesh_ip": "192.0.2.6", "bus": true, "status": "paused" }
                ]),
                "member[1]: unknown status \"paused\"",
            ),
        ];
        for (label, members, expected) in cases {
            let err = strict_routing_view(&members, SUBNET).expect_err(label);
            assert!(err.to_string().contains(expected), "{label}: {err}");
        }
    }

    #[test]
    fn enforces_spec01_label_grammar_for_every_member_status() {
        let long_name = "a".repeat(64);
        let cases = [
            (
                "uppercase-active",
                json!([
                    { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                    { "name": "Beta", "mesh_ip": "192.0.2.6", "bus": true, "status": "active" }
                ]),
                "Beta",
            ),
            (
                "leading-hyphen-tombstone",
                json!([
                    { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                    { "name": "-beta", "status": "tombstoned" }
                ]),
                "-beta",
            ),
            (
                "trailing-hyphen-transport",
                json!([
                    { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                    { "name": "beta-", "mesh_ip": "192.0.2.6", "bus": false, "status": "active" }
                ]),
                "beta-",
            ),
            (
                "overlong-tombstone",
                json!([
                    { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                    { "name": long_name, "status": "tombstoned" }
                ]),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ];

        for (case, members, rejected_name) in cases {
            let error = strict_routing_view(&members, SUBNET).expect_err(case);
            let message = error.to_string();
            assert!(
                message.contains(&format!("invalid bus label {rejected_name:?}"))
                    && message.contains("SPEC 01 §4.1"),
                "{case}: {message}"
            );
        }

        let view = strict_routing_view(
            &json!([
                { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                { "name": "beta-2", "mesh_ip": "192.0.2.6", "bus": true, "status": "active" },
                { "name": "gamma-3", "status": "tombstoned" }
            ]),
            SUBNET,
        )
        .expect("valid lowercase labels");
        assert_eq!(view.len(), 3);
    }

    #[test]
    fn rejects_active_address_outside_signed_subnet() {
        let err = strict_routing_view(
            &json!([
                { "name": "alpha", "mesh_ip": "198.51.100.5", "bus": true, "status": "active" }
            ]),
            SUBNET,
        )
        .expect_err("out-of-subnet route must reject");
        assert!(err.to_string().contains("outside inventory subnet"));
    }

    #[test]
    fn resolves_absent_and_boundary_active_bus_ports() {
        let view = strict_routing_view(
            &json!([
                { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                { "name": "beta", "mesh_ip": "192.0.2.6", "bus": true, "status": "active", "noded_port": 1 },
                { "name": "gamma", "mesh_ip": "192.0.2.7", "bus": true, "status": "active", "noded_port": 65535 }
            ]),
            SUBNET,
        )
        .expect("valid endpoint boundaries");

        assert!(matches!(
            &view[0],
            RoutingMember::ActiveBus {
                noded_port: DEFAULT_NODED_PORT,
                ..
            }
        ));
        assert!(matches!(
            &view[1],
            RoutingMember::ActiveBus { noded_port: 1, .. }
        ));
        assert!(matches!(
            &view[2],
            RoutingMember::ActiveBus {
                noded_port: 65535,
                ..
            }
        ));
        assert_eq!(view[1].to_json()["noded_port"], 1);
    }

    #[test]
    fn rejects_every_invalid_active_bus_port_shape() {
        let cases = [
            ("zero", json!(0)),
            ("too-large", json!(65536)),
            ("negative", json!(-1)),
            ("float", json!(4200.0)),
            ("string", json!("4200")),
            ("boolean", json!(true)),
            ("null", json!(null)),
            ("array", json!([4200])),
            ("object", json!({ "port": 4200 })),
        ];

        for (case, noded_port) in cases {
            let error = strict_routing_view(
                &json!([{
                    "name": "alpha",
                    "mesh_ip": "192.0.2.5",
                    "bus": true,
                    "status": "active",
                    "noded_port": noded_port
                }]),
                SUBNET,
            )
            .expect_err(case);
            assert!(
                error
                    .to_string()
                    .contains("noded_port must be a JSON integer in the range 1..=65535"),
                "{case}: {error}"
            );
        }
    }

    #[test]
    fn ignores_noded_port_on_non_active_bus_members() {
        let view = strict_routing_view(
            &json!([
                { "name": "alpha", "mesh_ip": "192.0.2.5", "bus": true, "status": "active" },
                { "name": "beta", "mesh_ip": "192.0.2.6", "bus": false, "status": "active", "noded_port": "not-a-port" },
                { "name": "gamma", "status": "tombstoned", "noded_port": 0 }
            ]),
            SUBNET,
        )
        .expect("route-free members do not own a broker endpoint");

        assert!(matches!(view[1], RoutingMember::TransportOnly { .. }));
        assert!(matches!(view[2], RoutingMember::Tombstoned { .. }));
        assert!(view[1].to_json().get("noded_port").is_none());
        assert!(view[2].to_json().get("noded_port").is_none());
    }

    #[test]
    fn rejects_zero_active_bus_members() {
        let err = strict_routing_view(
            &json!([
                { "name": "beta", "mesh_ip": "192.0.2.6", "bus": false, "status": "active" },
                { "name": "retired", "status": "tombstoned" }
            ]),
            SUBNET,
        )
        .expect_err("zero active bus must reject");
        assert_eq!(err, RoutingViewError::ZeroActiveBus);
    }
}
