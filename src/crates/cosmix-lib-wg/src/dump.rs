//! `wg show <iface> dump` parser → [`WgShowDump`] + [`PeerStatus`].
//!
//! Pure: takes the captured stdout of `wg show <iface> dump` (or
//! `wg show all dump` for a single interface filtered upstream) and
//! returns structured records. The actual shell-out lives in the
//! citizen-side reconciler (the §4.1 status-sync timer); core only
//! decodes what it is handed.
//!
//! Dump format (from `wg(8)`, tab-separated, NO header):
//! - **Line 1** (interface): `private-key\tpublic-key\tlisten-port\tfwmark`
//! - **Lines 2..N** (peers): `public-key\tpreshared-key\tendpoint\tallowed-ips\tlatest-handshake\trx\ttx\tpersistent-keepalive`
//!
//! Sentinels: `(none)` for missing preshared-key / endpoint / allowed-
//! ips; `0` for never-handshaked; `off` for disabled fwmark and
//! disabled keepalive. We **never** read the private-key field on the
//! interface line — it would be `(none)` for an unprivileged caller
//! anyway, and storing it would invert the secret-handling contract
//! (private keys live on disk under `secrets/<iface>.key`, never in a
//! daemon's in-memory peer list).

use crate::keys::WgPublicKey;

/// Connected-vs-offline threshold (seconds since latest handshake) —
/// matches wg-admin's `App\Enums\PeerStatus::fromHandshake()` 180s
/// cut-off, which is one rekey interval (2 min) plus a slack tick.
pub const CONNECTED_THRESHOLD_SECS: u64 = 180;

/// Derived peer status. **Not** a property — derived from netlink
/// each tick per plan §3 "Live status is not a property".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// No successful handshake has ever been recorded
    /// (`latest_handshake_unix == 0`). The peer is configured in the
    /// kernel but the remote has not yet completed a Curve25519
    /// exchange — typically a freshly-allocated peer that has not
    /// opened its WireGuard tunnel yet.
    Pending,
    /// A handshake completed within the last
    /// [`CONNECTED_THRESHOLD_SECS`] seconds.
    Connected,
    /// Has handshaked at some point, but not within the threshold.
    Offline,
}

impl PeerStatus {
    /// Classify a peer by its latest handshake timestamp (UNIX
    /// seconds; `0` means never) against a caller-supplied `now`.
    /// `now` is injected so the function is deterministic and
    /// testable; the citizen-side caller normally passes
    /// `SystemTime::now()` converted to UNIX seconds.
    ///
    /// `saturating_sub` defends against a future-dated handshake
    /// stamp (clock skew between the local host and the remote
    /// peer); we treat "handshake in the future" as Connected,
    /// matching the wg-admin behaviour of `time() - $handshake < 180`
    /// when the difference is negative.
    pub fn from_handshake(latest_handshake_unix: u64, now_unix: u64) -> Self {
        if latest_handshake_unix == 0 {
            return PeerStatus::Pending;
        }
        if now_unix.saturating_sub(latest_handshake_unix) < CONNECTED_THRESHOLD_SECS {
            PeerStatus::Connected
        } else {
            PeerStatus::Offline
        }
    }
}

/// Decoded `wg show <iface> dump` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgShowDump {
    /// Line 1 of the dump.
    pub interface: WgInterfaceDump,
    /// Lines 2..N — preserves wg's emission order (which is the
    /// kernel's `peers` list order, not lexicographic).
    pub peers: Vec<PeerDump>,
}

/// Interface-line fields from the dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgInterfaceDump {
    /// Public key (base64). The matching *private* key from the dump
    /// is intentionally dropped before this struct is constructed —
    /// see module docs.
    pub public_key: WgPublicKey,
    /// UDP listen port. `0` indicates no listen port set (e.g. a
    /// freshly-created interface before the daemon has assigned a
    /// port).
    pub listen_port: u16,
    /// fwmark, if set. `wg` emits `off` when the firewall mark is
    /// disabled, otherwise a hex value (`0xca6c`-style) or decimal
    /// depending on the wg-tools version — we accept both.
    pub fwmark: Option<u32>,
}

/// Per-peer fields from the dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDump {
    /// Peer's public key (base64). Matches the `current_pubkey` field
    /// in the SPEC-12 `peers` property record.
    pub public_key: WgPublicKey,
    /// `true` when a preshared key is configured for this peer. We
    /// intentionally do not capture the PSK value itself — the PSK is
    /// secret material that the wgd hub already holds in property
    /// storage; the dump value is redundant and would invert the
    /// secret-handling contract.
    pub has_preshared_key: bool,
    /// Last-known endpoint (`host:port` shape; IPv6 endpoints are
    /// bracketed by `wg`). `None` when the kernel reports `(none)`.
    pub endpoint: Option<String>,
    /// AllowedIPs split on `,`, with surrounding whitespace trimmed.
    /// Empty when wg emits `(none)`.
    pub allowed_ips: Vec<String>,
    /// UNIX timestamp of the most recent successful handshake. `0`
    /// when no handshake has ever completed (use [`PeerStatus`] for
    /// the derived classification rather than inspecting this
    /// directly).
    pub latest_handshake_unix: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Configured persistent-keepalive interval in seconds. `None`
    /// when wg emits `off` (keepalive disabled).
    pub persistent_keepalive: Option<u16>,
}

/// Errors from [`parse_wg_show_dump`].
///
/// `PartialEq`/`Eq` are intentionally not derived — `BadPublicKey`
/// carries a `KeyError` which transitively wraps `base64::DecodeError`
/// (not `Eq`). Tests pattern-match on variants instead of comparing.
#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    /// No non-empty lines.
    #[error("wg show dump is empty")]
    Empty,
    /// Interface line had the wrong number of tab-separated fields.
    #[error("interface line has {got} fields, expected 4")]
    BadInterfaceLine { got: usize },
    /// Peer line had the wrong number of tab-separated fields. `line`
    /// is 1-based, counting peer lines only (interface line is not
    /// counted).
    #[error("peer line {line} has {got} fields, expected 8")]
    BadPeerLine { line: usize, got: usize },
    /// Listen-port field on the interface line did not parse as a
    /// `u16`.
    #[error("invalid listen_port `{0}`")]
    BadListenPort(String),
    /// fwmark field on the interface line was neither `off` nor a
    /// valid decimal/hex `u32`.
    #[error("invalid fwmark `{0}`")]
    BadFwmark(String),
    /// A numeric peer field (handshake, rx, tx) did not parse.
    #[error("invalid {field} `{value}` on peer line {line}")]
    BadPeerField {
        field: &'static str,
        value: String,
        line: usize,
    },
    /// persistent-keepalive was neither `off` nor a valid `u16`.
    #[error("invalid persistent_keepalive `{value}` on peer line {line}")]
    BadKeepalive { value: String, line: usize },
    /// Public-key field (interface or peer) failed key-format validation.
    #[error("invalid public_key on {context}: {source}")]
    BadPublicKey {
        context: String,
        #[source]
        source: crate::keys::KeyError,
    },
}

/// Decode `wg show <iface> dump` output into [`WgShowDump`].
///
/// Empty trailing newlines and blank lines are tolerated. The
/// interface-line private-key field is parsed only to confirm field
/// count; its value is discarded before this function returns.
pub fn parse_wg_show_dump(text: &str) -> Result<WgShowDump, DumpError> {
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let iface_line = lines.next().ok_or(DumpError::Empty)?;
    let (public_key, listen_port, fwmark) = parse_interface_line(iface_line)?;

    let mut peers = Vec::new();
    for (i, line) in lines.enumerate() {
        let line_no = i + 1;
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 8 {
            return Err(DumpError::BadPeerLine {
                line: line_no,
                got: parts.len(),
            });
        }
        let public_key =
            WgPublicKey::from_base64(parts[0]).map_err(|e| DumpError::BadPublicKey {
                context: format!("peer line {line_no}"),
                source: e,
            })?;
        let has_preshared_key = parts[1] != "(none)";
        let endpoint = if parts[2] == "(none)" {
            None
        } else {
            Some(parts[2].to_string())
        };
        let allowed_ips = if parts[3] == "(none)" {
            Vec::new()
        } else {
            parts[3]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let latest_handshake_unix = parse_u64(parts[4], "latest_handshake", line_no)?;
        let rx_bytes = parse_u64(parts[5], "rx_bytes", line_no)?;
        let tx_bytes = parse_u64(parts[6], "tx_bytes", line_no)?;
        let persistent_keepalive = match parts[7] {
            "off" => None,
            s => Some(s.parse::<u16>().map_err(|_| DumpError::BadKeepalive {
                value: s.to_string(),
                line: line_no,
            })?),
        };

        peers.push(PeerDump {
            public_key,
            has_preshared_key,
            endpoint,
            allowed_ips,
            latest_handshake_unix,
            rx_bytes,
            tx_bytes,
            persistent_keepalive,
        });
    }

    Ok(WgShowDump {
        interface: WgInterfaceDump {
            public_key,
            listen_port,
            fwmark,
        },
        peers,
    })
}

/// Parse the interface line into `(public_key, listen_port, fwmark)`.
///
/// The interface line's first field is the private key. This helper
/// pulls fields out of the iterator one at a time so the
/// private-key field is taken via `.next()` into an unbound
/// `Option<&str>` and dropped before any other field is touched —
/// no `Vec<&str>` keeps a reference to it. The underlying `text`
/// buffer is still owned by the caller, but the parser itself
/// never binds the private-key field to a named local that could
/// be debug-formatted or leaked through a panic message.
fn parse_interface_line(line: &str) -> Result<(WgPublicKey, u16, Option<u32>), DumpError> {
    let mut fields = line.split('\t');
    // Field 0: private key. Take + drop without binding.
    if fields.next().is_none() {
        return Err(DumpError::BadInterfaceLine { got: 0 });
    }
    let public_key_field = fields
        .next()
        .ok_or(DumpError::BadInterfaceLine { got: 1 })?;
    let listen_port_field = fields
        .next()
        .ok_or(DumpError::BadInterfaceLine { got: 2 })?;
    let fwmark_field = fields
        .next()
        .ok_or(DumpError::BadInterfaceLine { got: 3 })?;
    // Reject extras: count remaining fields to give a useful error.
    let extras = fields.count();
    if extras > 0 {
        return Err(DumpError::BadInterfaceLine { got: 4 + extras });
    }

    let public_key =
        WgPublicKey::from_base64(public_key_field).map_err(|e| DumpError::BadPublicKey {
            context: "interface line".to_string(),
            source: e,
        })?;
    let listen_port: u16 = listen_port_field
        .parse()
        .map_err(|_| DumpError::BadListenPort(listen_port_field.to_string()))?;
    let fwmark = parse_fwmark(fwmark_field)?;
    Ok((public_key, listen_port, fwmark))
}

fn parse_fwmark(s: &str) -> Result<Option<u32>, DumpError> {
    match s {
        "off" => Ok(None),
        s if s.starts_with("0x") || s.starts_with("0X") => u32::from_str_radix(&s[2..], 16)
            .map(Some)
            .map_err(|_| DumpError::BadFwmark(s.to_string())),
        s => s
            .parse::<u32>()
            .map(Some)
            .map_err(|_| DumpError::BadFwmark(s.to_string())),
    }
}

fn parse_u64(s: &str, field: &'static str, line: usize) -> Result<u64, DumpError> {
    s.parse::<u64>().map_err(|_| DumpError::BadPeerField {
        field,
        value: s.to_string(),
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real-looking dump matching the format from `wg show <iface> dump`.
    // Interface line: private-key  public-key  listen-port  fwmark
    // Peer line:      public-key  preshared-key  endpoint  allowed-ips
    //                 latest-handshake  rx  tx  persistent-keepalive
    const IFACE_PRIV: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=";
    const IFACE_PUB: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA=";
    const PEER_A_PUB: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCA=";
    const PEER_B_PUB: &str = "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDA=";
    const PSK: &str = "EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEA=";

    fn full_dump() -> String {
        // Two peers: A connected with PSK + endpoint + keepalive,
        // B pending (handshake=0) without endpoint or keepalive.
        format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t{PSK}\t203.0.113.5:51820\t192.0.2.5/32,fd00::5/128\t1700000000\t1234\t5678\t25\n\
             {PEER_B_PUB}\t(none)\t(none)\t192.0.2.6/32\t0\t0\t0\toff\n"
        )
    }

    // -- PeerStatus::from_handshake -----------------------------------

    #[test]
    fn from_handshake_zero_is_pending() {
        assert_eq!(
            PeerStatus::from_handshake(0, 1_700_000_000),
            PeerStatus::Pending
        );
    }

    #[test]
    fn from_handshake_recent_is_connected() {
        let now = 1_700_000_000u64;
        // 179s < 180s → Connected
        assert_eq!(
            PeerStatus::from_handshake(now - 179, now),
            PeerStatus::Connected
        );
        // 0s diff is Connected (just-handshaked)
        assert_eq!(PeerStatus::from_handshake(now, now), PeerStatus::Connected);
    }

    #[test]
    fn from_handshake_at_threshold_is_offline() {
        // The boundary is `< 180`, so exactly 180 is Offline.
        let now = 1_700_000_000u64;
        assert_eq!(
            PeerStatus::from_handshake(now - 180, now),
            PeerStatus::Offline
        );
        assert_eq!(
            PeerStatus::from_handshake(now - 600, now),
            PeerStatus::Offline
        );
    }

    #[test]
    fn from_handshake_future_dated_is_connected() {
        // Clock skew: peer reports a handshake stamped *after* our
        // local now. `saturating_sub` returns 0 → Connected.
        let now = 1_700_000_000u64;
        assert_eq!(
            PeerStatus::from_handshake(now + 60, now),
            PeerStatus::Connected
        );
    }

    // -- parse_wg_show_dump happy paths -------------------------------

    #[test]
    fn parses_full_dump() {
        let dump = full_dump();
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.interface.public_key.to_base64(), IFACE_PUB);
        assert_eq!(parsed.interface.listen_port, 51820);
        assert_eq!(parsed.interface.fwmark, None);
        assert_eq!(parsed.peers.len(), 2);

        let a = &parsed.peers[0];
        assert_eq!(a.public_key.to_base64(), PEER_A_PUB);
        assert!(a.has_preshared_key);
        assert_eq!(a.endpoint.as_deref(), Some("203.0.113.5:51820"));
        assert_eq!(a.allowed_ips, vec!["192.0.2.5/32", "fd00::5/128"]);
        assert_eq!(a.latest_handshake_unix, 1_700_000_000);
        assert_eq!(a.rx_bytes, 1234);
        assert_eq!(a.tx_bytes, 5678);
        assert_eq!(a.persistent_keepalive, Some(25));

        let b = &parsed.peers[1];
        assert_eq!(b.public_key.to_base64(), PEER_B_PUB);
        assert!(!b.has_preshared_key);
        assert_eq!(b.endpoint, None);
        assert_eq!(b.allowed_ips, vec!["192.0.2.6/32"]);
        assert_eq!(b.latest_handshake_unix, 0);
        assert_eq!(b.persistent_keepalive, None);
        // And from_handshake classifies it Pending.
        assert_eq!(
            PeerStatus::from_handshake(b.latest_handshake_unix, 1_700_001_000),
            PeerStatus::Pending
        );
    }

    #[test]
    fn parses_interface_only_no_peers() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n");
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.peers, vec![]);
        assert_eq!(parsed.interface.listen_port, 51820);
    }

    #[test]
    fn tolerates_trailing_blank_lines() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\n\n");
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.peers, vec![]);
    }

    #[test]
    fn accepts_hex_fwmark() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\t0xca6c\n");
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.interface.fwmark, Some(0xca6c));
    }

    #[test]
    fn accepts_decimal_fwmark() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\t12345\n");
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.interface.fwmark, Some(12345));
    }

    #[test]
    fn listen_port_zero_is_valid() {
        // Freshly-created interface before listen-port assignment.
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t0\toff\n");
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.interface.listen_port, 0);
    }

    #[test]
    fn allowed_ips_none_yields_empty_vec() {
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t(none)\t(none)\t(none)\t0\t0\t0\toff\n"
        );
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(parsed.peers[0].allowed_ips, Vec::<String>::new());
    }

    #[test]
    fn interface_private_key_is_not_retained_anywhere() {
        // Structural assertion: there is no field on WgInterfaceDump
        // that could hold the private key. This test exists so a
        // future "store the private key for convenience" refactor
        // breaks a compile assertion rather than silently leaking.
        let _check_no_field: fn(&WgInterfaceDump) = |iface| {
            // If this list ever grows a `private_key` field, the
            // explicit destructure below stops compiling.
            let WgInterfaceDump {
                public_key: _,
                listen_port: _,
                fwmark: _,
            } = iface;
        };
    }

    // -- parse_wg_show_dump error paths -------------------------------

    #[test]
    fn rejects_empty_input() {
        match parse_wg_show_dump("") {
            Err(DumpError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
        match parse_wg_show_dump("\n\n") {
            Err(DumpError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }

    #[test]
    fn rejects_interface_line_with_wrong_field_count() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\n");
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadInterfaceLine { got: 3 }) => {}
            other => panic!("expected BadInterfaceLine{{got:3}}, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_listen_port() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\tnope\toff\n");
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadListenPort(s)) => assert_eq!(s, "nope"),
            other => panic!("expected BadListenPort, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_fwmark() {
        let dump = format!("{IFACE_PRIV}\t{IFACE_PUB}\t51820\tnotanumber\n");
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadFwmark(s)) => assert_eq!(s, "notanumber"),
            other => panic!("expected BadFwmark, got {other:?}"),
        }
    }

    #[test]
    fn rejects_peer_line_with_wrong_field_count() {
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t(none)\t(none)\t192.0.2.5/32\t0\t0\t0\n"
        );
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadPeerLine { line: 1, got: 7 }) => {}
            other => panic!("expected BadPeerLine{{line:1,got:7}}, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_handshake_value() {
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t(none)\t(none)\t192.0.2.5/32\tnope\t0\t0\toff\n"
        );
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadPeerField {
                field: "latest_handshake",
                value,
                line: 1,
            }) => assert_eq!(value, "nope"),
            other => panic!("expected BadPeerField{{handshake}}, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_keepalive() {
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t(none)\t(none)\t192.0.2.5/32\t0\t0\t0\t-5\n"
        );
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadKeepalive { value, line: 1 }) => assert_eq!(value, "-5"),
            other => panic!("expected BadKeepalive, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_iface_public_key() {
        let dump = format!("{IFACE_PRIV}\tnot-a-key\t51820\toff\n");
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadPublicKey { context, .. }) => {
                assert_eq!(context, "interface line");
            }
            other => panic!("expected BadPublicKey, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_peer_public_key() {
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             not-a-key\t(none)\t(none)\t192.0.2.5/32\t0\t0\t0\toff\n"
        );
        match parse_wg_show_dump(&dump) {
            Err(DumpError::BadPublicKey { context, .. }) => {
                assert_eq!(context, "peer line 1");
            }
            other => panic!("expected BadPublicKey, got {other:?}"),
        }
    }

    #[test]
    fn allowed_ips_trims_surrounding_whitespace() {
        // wg shouldn't emit padded commas but be permissive.
        let dump = format!(
            "{IFACE_PRIV}\t{IFACE_PUB}\t51820\toff\n\
             {PEER_A_PUB}\t(none)\t(none)\t 192.0.2.5/32 , fd00::5/128 \t0\t0\t0\toff\n"
        );
        let parsed = parse_wg_show_dump(&dump).unwrap();
        assert_eq!(
            parsed.peers[0].allowed_ips,
            vec!["192.0.2.5/32", "fd00::5/128"]
        );
    }
}
