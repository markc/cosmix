//! Flat `key: value` `.conf.mix` configuration, filesd style.
//!
//! `#` comments and blank lines are ignored, the key is split on the
//! first `:`, both sides trimmed. Repeated keys (`quota_owner:`) are
//! read from the raw text, not the flat map, so they cannot collide.
//!
//! Keys: `root`, `name`, `lane_bind`, `quota_total_bytes`,
//! `quota_owner_default_bytes`, `quota_owner: <owner>=<bytes>`
//! (repeatable). The byte values accept plain integers or a `KiB`
//! family suffix.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Default total store cap: 50 GiB.
pub const DEFAULT_QUOTA_TOTAL_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// Default per-owner cap: 10 GiB.
pub const DEFAULT_QUOTA_OWNER_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Parsed configuration with defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// mds root. `None` means the caller's default (the unit's
    /// `StateDirectory`, `/var/lib/cosmix/blobd`).
    pub root: Option<PathBuf>,
    /// Instance name: the Bus service is `blobd` or `blobd-<name>`,
    /// with a root per instance.
    pub name: Option<String>,
    /// Byte-lane bind `<ip>:<port>`. The lane itself is a later slice;
    /// here it is only validated and used to build `blob.url`.
    pub lane_bind: Option<SocketAddr>,
    pub quota_total_bytes: u64,
    pub quota_owner_default_bytes: u64,
    /// Per-owner caps from repeated `quota_owner: <owner>=<bytes>`
    /// lines. Later lines for the same owner win.
    pub owner_limits: BTreeMap<String, u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: None,
            name: None,
            lane_bind: None,
            quota_total_bytes: DEFAULT_QUOTA_TOTAL_BYTES,
            quota_owner_default_bytes: DEFAULT_QUOTA_OWNER_BYTES,
            owner_limits: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Parse a config file's text over the defaults. Unknown keys are
    /// ignored (forward-compatible, filesd's posture); malformed values
    /// for known keys are errors.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("quota_owner:") {
                let body = line.trim_start_matches("quota_owner:").trim();
                let (owner, bytes) = body.split_once('=').ok_or_else(|| {
                    format!("quota_owner: expected <owner>=<bytes>, got {body:?}")
                })?;
                let owner = owner.trim();
                if owner.is_empty() {
                    return Err(format!("quota_owner: empty owner in {body:?}"));
                }
                let bytes = parse_bytes(bytes.trim())
                    .ok_or_else(|| format!("quota_owner: bad byte size in {body:?}"))?;
                cfg.owner_limits.insert(owner.to_string(), bytes);
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "root" => cfg.root = Some(PathBuf::from(v)),
                "name" => {
                    if v.is_empty() {
                        return Err("name: empty instance name".into());
                    }
                    cfg.name = Some(v.to_string());
                }
                "lane_bind" => {
                    let addr: SocketAddr = v
                        .parse()
                        .map_err(|e| format!("lane_bind: {v:?} is not <ip>:<port>: {e}"))?;
                    cfg.lane_bind = Some(addr);
                }
                "quota_total_bytes" => {
                    cfg.quota_total_bytes = parse_bytes(v)
                        .ok_or_else(|| format!("quota_total_bytes: bad byte size {v:?}"))?;
                }
                "quota_owner_default_bytes" => {
                    cfg.quota_owner_default_bytes = parse_bytes(v)
                        .ok_or_else(|| format!("quota_owner_default_bytes: bad byte size {v:?}"))?;
                }
                _ => {}
            }
        }
        Ok(cfg)
    }

    /// The Bus service name for this instance.
    pub fn service_name(&self) -> String {
        match &self.name {
            Some(name) => format!("blobd-{name}"),
            None => "blobd".to_string(),
        }
    }
}

/// Parse a byte count: a plain integer, or an integer with a binary
/// suffix (`KiB`, `MiB`, `GiB`, `TiB`; the `i` is optional and `KB`
/// style is still binary — this is a byte-count field, not a rate).
pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let n: u64 = digits.trim().parse().ok()?;
    let mul = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024u64 * 1024 * 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mul)
}

/// Alias documenting quota fields in store types.
pub type ByteSize = u64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_yields_defaults() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.service_name(), "blobd");
        assert_eq!(cfg.quota_total_bytes, DEFAULT_QUOTA_TOTAL_BYTES);
        assert_eq!(cfg.quota_owner_default_bytes, DEFAULT_QUOTA_OWNER_BYTES);
    }

    #[test]
    fn parses_every_key() {
        let cfg = Config::parse(
            "# comment\nroot: /var/lib/cosmix/blobd-two\nname: two\n\
             lane_bind: 10.42.0.5:4210\nquota_total_bytes: 100GiB\n\
             quota_owner_default_bytes: 512MiB\nquota_owner: maild=1GiB\n\
             quota_owner: capture=2 GiB\nignored_key: whatever\n",
        )
        .unwrap();
        assert_eq!(cfg.root, Some(PathBuf::from("/var/lib/cosmix/blobd-two")));
        assert_eq!(cfg.name.as_deref(), Some("two"));
        assert_eq!(cfg.service_name(), "blobd-two");
        assert_eq!(
            cfg.lane_bind.unwrap().to_string(),
            "10.42.0.5:4210".to_string()
        );
        assert_eq!(cfg.quota_total_bytes, 100 * 1024 * 1024 * 1024);
        assert_eq!(cfg.quota_owner_default_bytes, 512 * 1024 * 1024);
        assert_eq!(cfg.owner_limits["maild"], 1024 * 1024 * 1024);
        assert_eq!(cfg.owner_limits["capture"], 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn repeated_owner_lines_last_wins() {
        let cfg = Config::parse("quota_owner: a=1MiB\nquota_owner: a=3MiB\n").unwrap();
        assert_eq!(cfg.owner_limits["a"], 3 * 1024 * 1024);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("lane_bind: not-an-addr\n").is_err());
        assert!(Config::parse("lane_bind: 10.42.0.5\n").is_err());
        assert!(Config::parse("quota_total_bytes: lots\n").is_err());
        assert!(Config::parse("quota_owner: noequals\n").is_err());
        assert!(Config::parse("quota_owner: =5MiB\n").is_err());
        assert!(Config::parse("name:\n").is_err());
    }

    #[test]
    fn ipv6_lane_bind_parses() {
        let cfg = Config::parse("lane_bind: [fd00::5]:4210\n").unwrap();
        assert!(cfg.lane_bind.is_some());
    }

    #[test]
    fn byte_suffix_table() {
        assert_eq!(parse_bytes("0"), Some(0));
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("1KiB"), Some(1024));
        assert_eq!(parse_bytes("1kb"), Some(1024));
        assert_eq!(parse_bytes("2 MiB"), Some(2 * 1024 * 1024));
        assert_eq!(parse_bytes("1GiB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_bytes("1TiB"), Some(1024u64 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("-1"), None);
        assert_eq!(parse_bytes("1.5GiB"), None);
        assert_eq!(parse_bytes("1PiB"), None);
        assert_eq!(parse_bytes(""), None);
    }
}
