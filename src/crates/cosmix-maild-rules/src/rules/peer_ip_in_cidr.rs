//! `peer_ip_in_cidr` — matches when `RuleContext::peer_ip` falls in any
//! of the configured CIDR blocks.
//!
//! Compilation parses each CIDR string into `(network, prefix_len)`;
//! match-time uses bit masking, no allocation.

use std::net::IpAddr;

use crate::compiled::MatchInputs;

#[derive(Debug, Clone)]
pub struct PeerIpInCidr {
    pub cidrs: Vec<Cidr>,
}

#[derive(Debug, Clone, Copy)]
pub enum Cidr {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix_str) = s.split_once('/')?;
        let prefix: u8 = prefix_str.parse().ok()?;
        let ip: IpAddr = addr.parse().ok()?;
        match ip {
            IpAddr::V4(v4) => {
                if prefix > 32 {
                    return None;
                }
                let network = u32::from_be_bytes(v4.octets());
                let mask = if prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - prefix)
                };
                Some(Cidr::V4 {
                    network: network & mask,
                    prefix,
                })
            }
            IpAddr::V6(v6) => {
                if prefix > 128 {
                    return None;
                }
                let network = u128::from_be_bytes(v6.octets());
                let mask = if prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - prefix)
                };
                Some(Cidr::V6 {
                    network: network & mask,
                    prefix,
                })
            }
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Cidr::V4 { network, prefix }, IpAddr::V4(v4)) => {
                let host = u32::from_be_bytes(v4.octets());
                let mask = if *prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - *prefix)
                };
                (host & mask) == *network
            }
            (Cidr::V6 { network, prefix }, IpAddr::V6(v6)) => {
                let host = u128::from_be_bytes(v6.octets());
                let mask = if *prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - *prefix)
                };
                (host & mask) == *network
            }
            _ => false,
        }
    }
}

impl PeerIpInCidr {
    pub(crate) fn matches(&self, inputs: &MatchInputs<'_>) -> bool {
        self.cidrs.iter().any(|c| c.contains(inputs.peer_ip))
    }
}
