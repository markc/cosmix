//! Pure-logic IPAM for cosmix-wgd's `peer.allocate` flow:
//!
//! - [`Cidr`] — parsed `network/prefix` value type, IPv4 or IPv6.
//! - [`parse_cidr`] — strict parser (network bits must be zeroed; bad
//!   prefix lengths rejected).
//! - [`next_free_host`] — finds the lowest unused host address in a
//!   subnet given a `taken` slice (in-memory peer table from the
//!   reconciler).
//!
//! No I/O, no Cosmix-internal deps, no dependency on `ipnet`/`ipnetwork`
//! (the std::net types plus a few `u32`/`u128` casts cover the surface
//! we need). The reconciler holds the per-namespace write lock around
//! the read-IPAM-write sequence (`_doc/planned/cosmix-wgd.md` §5.3), so
//! this module never needs to know about concurrency.
//!
//! Host-counting convention (matching common WireGuard mesh practice):
//! - IPv4 `/n` where `n <= 30`: hosts are network+1 … broadcast-1.
//!   That is the same convention `wg-admin` follows when it picks the
//!   next address in a `/24`.
//! - IPv4 `/31` (RFC 3021): both addresses are hosts.
//! - IPv4 `/32`: the single network address is the host.
//! - IPv6 `/n` where `n <= 127`: hosts are network+1 … last (no
//!   broadcast in IPv6).
//! - IPv6 `/128`: the single network address is the host.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A parsed CIDR. Held as the network address (host bits cleared during
/// parse) plus the prefix length so iteration arithmetic is unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    /// The network address — host bits are guaranteed zero by parse.
    pub network: IpAddr,
    /// Prefix length in bits. 0 ≤ prefix ≤ 32 for v4, 0 ≤ prefix ≤ 128 for v6.
    pub prefix_len: u8,
}

impl Cidr {
    /// True iff `ip` falls inside this subnet — i.e. `ip` masked to
    /// `prefix_len` equals the network address. A **family mismatch**
    /// (a v4 `ip` against a v6 subnet, or vice-versa) is always `false`,
    /// never a panic. Used by consumers deriving a WG peer set from a
    /// signed inventory to reject a member whose `mesh_ip` is not in the
    /// mesh subnet before it reaches a peer stanza (a subnet/host mixup or
    /// a hostile record). A `Cidr` built by hand with an out-of-range
    /// prefix (v4 `/33`) yields `false` rather than panicking.
    pub fn contains(&self, ip: &IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                if self.prefix_len > 32 {
                    return false;
                }
                let net = u32::from_be_bytes(net.octets());
                let ip = u32::from_be_bytes(ip.octets());
                let mask = if self.prefix_len == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix_len as u32)
                };
                (ip & mask) == (net & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                if self.prefix_len > 128 {
                    return false;
                }
                let net = u128::from_be_bytes(net.octets());
                let ip = u128::from_be_bytes(ip.octets());
                let mask = if self.prefix_len == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix_len as u32)
                };
                (ip & mask) == (net & mask)
            }
            // Family mismatch — a v4 address is never inside a v6 subnet.
            _ => false,
        }
    }
}

/// Errors from [`parse_cidr`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CidrError {
    /// String had no `/`, or had more than one.
    #[error("malformed CIDR (expected `address/prefix`)")]
    Malformed,
    /// The address portion failed `std::net` parsing.
    #[error("invalid IP address: `{0}`")]
    BadAddress(String),
    /// The prefix portion failed `u8` parsing or was out of range for
    /// the address family.
    #[error("invalid prefix length `{got}` for IPv{family} (max {max})")]
    BadPrefix {
        /// The prefix string the caller passed (may not parse as u8).
        got: String,
        /// `4` or `6`.
        family: u8,
        /// 32 for v4, 128 for v6.
        max: u8,
    },
    /// One or more host bits were set in the input address. We refuse
    /// rather than silently masking because a stored `192.0.2.5/24`
    /// almost always means the operator mixed up host and subnet
    /// notation, and the substrate stores both kinds of CIDR (peer
    /// `/32`s vs interface subnets) — silently masking would erase the
    /// distinction.
    #[error("network bits set in host portion of `{addr}/{prefix}`")]
    HostBitsSet { addr: IpAddr, prefix: u8 },
}

/// Parse `addr/prefix`. Whitespace must be trimmed by the caller; we do
/// not trim implicitly because property values are stored verbatim and a
/// leading-space CIDR is a substrate-level data bug we want surfaced.
pub fn parse_cidr(s: &str) -> Result<Cidr, CidrError> {
    let (addr_str, prefix_str) = s.split_once('/').ok_or(CidrError::Malformed)?;
    if prefix_str.contains('/') {
        return Err(CidrError::Malformed);
    }
    let addr: IpAddr = addr_str
        .parse()
        .map_err(|_| CidrError::BadAddress(addr_str.to_string()))?;
    let max_bits = match addr {
        IpAddr::V4(_) => 32u8,
        IpAddr::V6(_) => 128u8,
    };
    let family = if matches!(addr, IpAddr::V4(_)) {
        4u8
    } else {
        6u8
    };
    let prefix_len: u8 = prefix_str.parse().map_err(|_| CidrError::BadPrefix {
        got: prefix_str.to_string(),
        family,
        max: max_bits,
    })?;
    if prefix_len > max_bits {
        return Err(CidrError::BadPrefix {
            got: prefix_str.to_string(),
            family,
            max: max_bits,
        });
    }
    if !host_bits_zero(addr, prefix_len) {
        return Err(CidrError::HostBitsSet {
            addr,
            prefix: prefix_len,
        });
    }
    Ok(Cidr {
        network: addr,
        prefix_len,
    })
}

/// True iff every bit below `prefix_len` in `addr` is zero. Used by
/// [`parse_cidr`] to reject the common "wrote the host address but said
/// /24" mistake.
fn host_bits_zero(addr: IpAddr, prefix_len: u8) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let bits = u32::from_be_bytes(v4.octets());
            host_bits_zero_u32(bits, prefix_len)
        }
        IpAddr::V6(v6) => {
            let bits = u128::from_be_bytes(v6.octets());
            host_bits_zero_u128(bits, prefix_len)
        }
    }
}

fn host_bits_zero_u32(bits: u32, prefix_len: u8) -> bool {
    if prefix_len == 32 {
        return true;
    }
    let host_mask = (1u64 << (32 - prefix_len as u32)).wrapping_sub(1) as u32;
    (bits & host_mask) == 0
}

fn host_bits_zero_u128(bits: u128, prefix_len: u8) -> bool {
    if prefix_len == 128 {
        return true;
    }
    let shift = 128 - prefix_len as u32;
    // Use a 256-bit shift via two halves to avoid the `1 << 128` panic.
    let host_mask = if shift == 128 {
        u128::MAX
    } else {
        (1u128 << shift) - 1
    };
    (bits & host_mask) == 0
}

/// Find the lowest unused host address in `subnet`, ignoring any address
/// already present in `taken`. Returns `None` when the subnet is full.
///
/// `taken` does not need to be sorted, and may contain addresses outside
/// `subnet` (we filter); duplicates are tolerated. Linear in
/// `taken.len() * subnet_hosts` worst case — acceptable for the small
/// mesh sizes wgd targets (≤ 254 peers per `/24`). If we ever need to
/// scale, switch to sorting `taken` and walking both in step.
///
/// Convention matches the module docs: v4 `/n` with `n <= 30` skips
/// network and broadcast; `/31` includes both; `/32` includes the lone
/// address; v6 always skips only the network address.
pub fn next_free_host(subnet: &Cidr, taken: &[IpAddr]) -> Option<IpAddr> {
    // `Cidr` fields are public for value-type ergonomics; that means a
    // caller can hand-build one with an out-of-range or family-mismatched
    // prefix (v4 `/33`, v6 `/129`) or with host bits set (e.g.
    // `255.255.255.255/0`, which would overflow `net_bits + 1` below).
    // Refuse rather than panic — `parse_cidr` enforces the same
    // invariants on the construction path.
    match subnet.network {
        IpAddr::V4(net) => {
            if subnet.prefix_len > 32 {
                return None;
            }
            if !host_bits_zero(subnet.network, subnet.prefix_len) {
                return None;
            }
            next_free_host_v4(net, subnet.prefix_len, taken)
        }
        IpAddr::V6(net) => {
            if subnet.prefix_len > 128 {
                return None;
            }
            if !host_bits_zero(subnet.network, subnet.prefix_len) {
                return None;
            }
            next_free_host_v6(net, subnet.prefix_len, taken)
        }
    }
}

fn next_free_host_v4(net: Ipv4Addr, prefix: u8, taken: &[IpAddr]) -> Option<IpAddr> {
    let net_bits = u32::from_be_bytes(net.octets());
    let (first, last) = v4_host_range(net_bits, prefix);
    for host in first..=last {
        let candidate = IpAddr::V4(Ipv4Addr::from(host.to_be_bytes()));
        if !taken.contains(&candidate) {
            return Some(candidate);
        }
        if host == u32::MAX {
            // Saturated — avoid an `i+1` overflow when prefix=0 and the
            // last address is 255.255.255.255. (For prefix=0 we also
            // hit this naturally; saturation is the same condition.)
            break;
        }
    }
    None
}

fn next_free_host_v6(net: Ipv6Addr, prefix: u8, taken: &[IpAddr]) -> Option<IpAddr> {
    let net_bits = u128::from_be_bytes(net.octets());
    let (first, last) = v6_host_range(net_bits, prefix);
    let mut host = first;
    loop {
        let candidate = IpAddr::V6(Ipv6Addr::from(host.to_be_bytes()));
        if !taken.contains(&candidate) {
            return Some(candidate);
        }
        if host == last {
            return None;
        }
        host += 1;
    }
}

/// Inclusive `(first, last)` host range for the IPv4 subnet.
fn v4_host_range(net_bits: u32, prefix: u8) -> (u32, u32) {
    match prefix {
        32 => (net_bits, net_bits),
        31 => (net_bits, net_bits | 1),
        n => {
            let host_mask = (1u64 << (32 - n as u32)).wrapping_sub(1) as u32;
            let broadcast = net_bits | host_mask;
            (net_bits + 1, broadcast - 1)
        }
    }
}

/// Inclusive `(first, last)` host range for the IPv6 subnet.
fn v6_host_range(net_bits: u128, prefix: u8) -> (u128, u128) {
    if prefix == 128 {
        return (net_bits, net_bits);
    }
    let shift = 128 - prefix as u32;
    let host_mask = if shift == 128 {
        u128::MAX
    } else {
        (1u128 << shift) - 1
    };
    let last = net_bits | host_mask;
    // IPv6 has no broadcast; the only reserved address is the network
    // itself (RFC 4291 §2.6.1). First usable host is net + 1.
    (net_bits.saturating_add(1), last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }
    fn ipv6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    // -- Cidr::contains -----------------------------------------------

    #[test]
    fn contains_matches_hosts_in_subnet_and_rejects_outside_and_family_mismatch() {
        let net = parse_cidr("192.0.2.0/24").unwrap();
        assert!(net.contains(&ipv4("192.0.2.1")));
        assert!(net.contains(&ipv4("192.0.2.254")));
        assert!(net.contains(&ipv4("192.0.2.0"))); // the network address itself is "in" the CIDR
        assert!(!net.contains(&ipv4("192.0.3.1")), "adjacent /24 is outside");
        assert!(!net.contains(&ipv4("10.0.0.1")));
        // Family mismatch never panics — a v6 address is not in a v4 subnet.
        assert!(!net.contains(&ipv6("fd00::1")));

        // /32 contains only itself; /0 contains everything of its family.
        let host = parse_cidr("192.0.2.5/32").unwrap();
        assert!(host.contains(&ipv4("192.0.2.5")));
        assert!(!host.contains(&ipv4("192.0.2.6")));
        let any = parse_cidr("0.0.0.0/0").unwrap();
        assert!(any.contains(&ipv4("203.0.113.9")));

        // A hand-built out-of-range prefix yields false, not a panic.
        let bogus = Cidr {
            network: ipv4("192.0.2.0"),
            prefix_len: 33,
        };
        assert!(!bogus.contains(&ipv4("192.0.2.1")));
    }

    // -- parse_cidr ---------------------------------------------------

    #[test]
    fn parses_v4_24() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        assert_eq!(c.network, ipv4("192.0.2.0"));
        assert_eq!(c.prefix_len, 24);
    }

    #[test]
    fn parses_v6_64() {
        let c = parse_cidr("fd00:1::/64").unwrap();
        assert_eq!(c.network, ipv6("fd00:1::"));
        assert_eq!(c.prefix_len, 64);
    }

    #[test]
    fn parses_v4_32_host() {
        let c = parse_cidr("192.0.2.5/32").unwrap();
        assert_eq!(c.network, ipv4("192.0.2.5"));
        assert_eq!(c.prefix_len, 32);
    }

    #[test]
    fn parses_v6_128_host() {
        let c = parse_cidr("fd00:2::5/128").unwrap();
        assert_eq!(c.network, ipv6("fd00:2::5"));
        assert_eq!(c.prefix_len, 128);
    }

    #[test]
    fn rejects_missing_slash() {
        assert_eq!(parse_cidr("192.0.2.0"), Err(CidrError::Malformed));
    }

    #[test]
    fn rejects_extra_slash() {
        assert_eq!(parse_cidr("192.0.2.0/24/8"), Err(CidrError::Malformed));
    }

    #[test]
    fn rejects_bad_address() {
        match parse_cidr("not-an-ip/24") {
            Err(CidrError::BadAddress(s)) => assert_eq!(s, "not-an-ip"),
            other => panic!("expected BadAddress, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_prefix_string() {
        match parse_cidr("192.0.2.0/abc") {
            Err(CidrError::BadPrefix { got, family, max }) => {
                assert_eq!(got, "abc");
                assert_eq!(family, 4);
                assert_eq!(max, 32);
            }
            other => panic!("expected BadPrefix, got {other:?}"),
        }
    }

    #[test]
    fn rejects_prefix_too_large_v4() {
        match parse_cidr("192.0.2.0/33") {
            Err(CidrError::BadPrefix { max, family, .. }) => {
                assert_eq!(max, 32);
                assert_eq!(family, 4);
            }
            other => panic!("expected BadPrefix, got {other:?}"),
        }
    }

    #[test]
    fn rejects_prefix_too_large_v6() {
        match parse_cidr("fd00::/129") {
            Err(CidrError::BadPrefix { max, family, .. }) => {
                assert_eq!(max, 128);
                assert_eq!(family, 6);
            }
            other => panic!("expected BadPrefix, got {other:?}"),
        }
    }

    #[test]
    fn rejects_host_bits_set_v4() {
        // 192.0.2.5/24 has host bits set (.5 in the host portion).
        match parse_cidr("192.0.2.5/24") {
            Err(CidrError::HostBitsSet { addr, prefix }) => {
                assert_eq!(addr, ipv4("192.0.2.5"));
                assert_eq!(prefix, 24);
            }
            other => panic!("expected HostBitsSet, got {other:?}"),
        }
    }

    #[test]
    fn rejects_host_bits_set_v6() {
        match parse_cidr("fd00::1/64") {
            Err(CidrError::HostBitsSet { prefix, .. }) => assert_eq!(prefix, 64),
            other => panic!("expected HostBitsSet, got {other:?}"),
        }
    }

    #[test]
    fn allows_prefix_0_v4_zero_address() {
        let c = parse_cidr("0.0.0.0/0").unwrap();
        assert_eq!(c.prefix_len, 0);
    }

    #[test]
    fn allows_prefix_0_v6_zero_address() {
        let c = parse_cidr("::/0").unwrap();
        assert_eq!(c.prefix_len, 0);
    }

    // -- next_free_host (IPv4) ---------------------------------------

    #[test]
    fn allocates_first_host_in_v4_24() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv4("192.0.2.1")));
    }

    #[test]
    fn skips_taken_addresses() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        let taken = [ipv4("192.0.2.1"), ipv4("192.0.2.2"), ipv4("192.0.2.4")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.3")));
    }

    #[test]
    fn ignores_out_of_subnet_taken_addresses() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        // 10.0.0.1 is not in 192.0.2.0/24, so it must not displace .1.
        let taken = [ipv4("10.0.0.1")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.1")));
    }

    #[test]
    fn skips_broadcast_and_network_in_v4_classic() {
        let c = parse_cidr("192.0.2.0/30").unwrap();
        // /30: network=.0, hosts=.1, .2, broadcast=.3
        assert_eq!(next_free_host(&c, &[]), Some(ipv4("192.0.2.1")));
        let taken = [ipv4("192.0.2.1")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.2")));
        let taken = [ipv4("192.0.2.1"), ipv4("192.0.2.2")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    #[test]
    fn rfc_3021_v4_31_yields_both_addresses() {
        let c = parse_cidr("10.0.0.0/31").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv4("10.0.0.0")));
        let taken = [ipv4("10.0.0.0")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("10.0.0.1")));
        let taken = [ipv4("10.0.0.0"), ipv4("10.0.0.1")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    #[test]
    fn v4_32_yields_the_single_host() {
        let c = parse_cidr("10.0.0.5/32").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv4("10.0.0.5")));
        let taken = [ipv4("10.0.0.5")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    #[test]
    fn returns_none_when_v4_subnet_full() {
        let c = parse_cidr("192.0.2.0/30").unwrap();
        let taken = [ipv4("192.0.2.1"), ipv4("192.0.2.2")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    #[test]
    fn allocates_around_hub_at_dot_one() {
        // The mesh hub conventionally takes .1; IPAM then hands out .2,
        // .3, … which is the wg-admin allocation order.
        let c = parse_cidr("192.0.2.0/24").unwrap();
        let taken = [ipv4("192.0.2.1")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.2")));
    }

    // -- next_free_host (IPv6) ---------------------------------------

    #[test]
    fn allocates_first_host_in_v6_64() {
        let c = parse_cidr("fd00:1::/64").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv6("fd00:1::1")));
    }

    #[test]
    fn skips_taken_v6() {
        let c = parse_cidr("fd00:1::/64").unwrap();
        let taken = [ipv6("fd00:1::1")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv6("fd00:1::2")));
    }

    #[test]
    fn v6_128_yields_single_host() {
        let c = parse_cidr("fd00:2::5/128").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv6("fd00:2::5")));
        let taken = [ipv6("fd00:2::5")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    #[test]
    fn v6_127_skips_only_network() {
        // Per the module docs, v6 skips only the network address
        // regardless of prefix (no broadcast). /127 therefore yields
        // exactly one host (the upper address).
        let c = parse_cidr("fd00::/127").unwrap();
        assert_eq!(next_free_host(&c, &[]), Some(ipv6("fd00::1")));
        let taken = [ipv6("fd00::1")];
        assert_eq!(next_free_host(&c, &taken), None);
    }

    // -- ordering & duplicate-tolerance properties --------------------

    #[test]
    fn allocation_is_lowest_unused() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        let taken = [
            ipv4("192.0.2.5"),
            ipv4("192.0.2.1"),
            ipv4("192.0.2.3"),
            ipv4("192.0.2.2"),
        ];
        // Lowest unused is .4, not the "next after the last taken".
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.4")));
    }

    #[test]
    fn duplicates_in_taken_are_tolerated() {
        let c = parse_cidr("192.0.2.0/24").unwrap();
        let taken = [ipv4("192.0.2.1"), ipv4("192.0.2.1"), ipv4("192.0.2.1")];
        assert_eq!(next_free_host(&c, &taken), Some(ipv4("192.0.2.2")));
    }

    // -- defence against hand-built malformed `Cidr` ------------------

    #[test]
    fn hand_built_cidr_with_oversize_v4_prefix_returns_none_not_panic() {
        // parse_cidr would reject /33, but the struct is public; we
        // refuse rather than panic in the shift arithmetic.
        let c = Cidr {
            network: ipv4("10.0.0.0"),
            prefix_len: 33,
        };
        assert_eq!(next_free_host(&c, &[]), None);
    }

    #[test]
    fn hand_built_cidr_with_oversize_v6_prefix_returns_none_not_panic() {
        let c = Cidr {
            network: ipv6("fd00::"),
            prefix_len: 129,
        };
        assert_eq!(next_free_host(&c, &[]), None);
    }

    #[test]
    fn hand_built_cidr_with_host_bits_set_returns_none_not_panic() {
        // 255.255.255.255/0 would overflow `net_bits + 1` in
        // v4_host_range; we refuse before reaching it.
        let c = Cidr {
            network: ipv4("255.255.255.255"),
            prefix_len: 0,
        };
        assert_eq!(next_free_host(&c, &[]), None);

        // Host bits set in the middle of a /24 — same refusal.
        let c = Cidr {
            network: ipv4("10.0.0.5"),
            prefix_len: 24,
        };
        assert_eq!(next_free_host(&c, &[]), None);

        // IPv6 sibling.
        let c = Cidr {
            network: ipv6("fd00::5"),
            prefix_len: 64,
        };
        assert_eq!(next_free_host(&c, &[]), None);
    }
}
