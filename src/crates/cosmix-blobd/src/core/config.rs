//! Flat `key: value` `.conf.mix` configuration, filesd style.
//!
//! `#` comments and blank lines are ignored, the key is split on the
//! first `:`, both sides trimmed. Repeated keys (`quota_owner:`) are
//! read from the raw text, not the flat map, so they cannot collide.
//!
//! Keys: `root`, `name`, `lane_bind`, `lane_max_uploads`,
//! `fetch_max_concurrent`, `fetch_queue_max`, `verb_max_concurrent`,
//! `quota_total_bytes`, `quota_owner_default_bytes`,
//! `quota_owner: <owner>=<bytes>` (repeatable). The byte values accept
//! plain integers or a `KiB` family suffix.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Default total store cap: 50 GiB.
pub const DEFAULT_QUOTA_TOTAL_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// Default per-owner cap: 10 GiB.
pub const DEFAULT_QUOTA_OWNER_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Default concurrent lane uploads: 4. Beyond it the lane answers 503
/// immediately — no queueing (the no-poll/no-flood law).
pub const DEFAULT_LANE_MAX_UPLOADS: usize = 4;
/// Default concurrent `blob.fetch` downloads: 2. Beyond it a fetch
/// queues in-process up to `fetch_queue_max`; the verb reply stays
/// immediate either way.
pub const DEFAULT_FETCH_MAX_CONCURRENT: usize = 2;
/// Default in-process fetch queue depth: 32. A fetch arriving beyond
/// it is refused rc 10 `busy` — never an unbounded queue.
pub const DEFAULT_FETCH_QUEUE_MAX: usize = 32;
/// Default concurrent verb dispatches: 8. A slow verb (a multi-GiB
/// `blob.put`, a CAS-walking `blob.gc`) holds one permit, not the
/// connection: beyond the bound a verb waits (its reply is late,
/// never lost) instead of blocking every other verb past the 30 s
/// mesh timeout (M2).
pub const DEFAULT_VERB_MAX_CONCURRENT: usize = 8;
/// Default CAS shared-read group (SPEC 10a §3.3): blobd chgrps its
/// state root and CAS root to this group with the setgid bit at open,
/// so members read `blob.path` targets and the daemon's supplementary
/// groups must include it (the unit's `SupplementaryGroups` line).
pub const DEFAULT_CAS_GROUP: &str = "cosmix-blob";

/// Parsed configuration with defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// mds root. `None` means the caller's default (the unit's
    /// `StateDirectory`, `/var/lib/cosmix/blobd`).
    pub root: Option<PathBuf>,
    /// Instance name: the Bus service is `blobd` or `blobd-<name>`,
    /// with a root per instance.
    pub name: Option<String>,
    /// Byte-lane bind `<ip>:<port>`. The listener exists only when
    /// this is set, and only after `bind_is_wg` has proved the IP is
    /// this node's `wg_ip` (fail closed, exit 2).
    pub lane_bind: Option<SocketAddr>,
    /// Concurrent lane uploads admitted at once; beyond it, 503.
    pub lane_max_uploads: usize,
    /// Concurrent `blob.fetch` downloads; beyond it a fetch queues
    /// in-process (up to `fetch_queue_max`).
    pub fetch_max_concurrent: usize,
    /// In-process fetch queue depth; a fetch beyond it is refused
    /// rc 10 `busy`.
    pub fetch_queue_max: usize,
    /// Concurrent verb dispatches; beyond it a verb queues (bounded,
    /// in-process) rather than blocking every other verb.
    pub verb_max_concurrent: usize,
    /// The shared-read group the state root and CAS root are chgrped
    /// to at open, with the setgid bit (mode 2750) so shard
    /// directories and CAS files inherit it (M4).
    pub cas_group: String,
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
            lane_max_uploads: DEFAULT_LANE_MAX_UPLOADS,
            fetch_max_concurrent: DEFAULT_FETCH_MAX_CONCURRENT,
            fetch_queue_max: DEFAULT_FETCH_QUEUE_MAX,
            verb_max_concurrent: DEFAULT_VERB_MAX_CONCURRENT,
            cas_group: DEFAULT_CAS_GROUP.to_string(),
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
                "lane_max_uploads" => {
                    let n: usize = v
                        .parse()
                        .map_err(|e| format!("lane_max_uploads: bad count {v:?}: {e}"))?;
                    if n == 0 {
                        return Err(
                            "lane_max_uploads: must be at least 1 (0 would refuse every upload)"
                                .into(),
                        );
                    }
                    cfg.lane_max_uploads = n;
                }
                "fetch_max_concurrent" => {
                    let n: usize = v
                        .parse()
                        .map_err(|e| format!("fetch_max_concurrent: bad count {v:?}: {e}"))?;
                    if n == 0 {
                        return Err(
                            "fetch_max_concurrent: must be at least 1 (0 would stall every fetch)"
                                .into(),
                        );
                    }
                    cfg.fetch_max_concurrent = n;
                }
                "fetch_queue_max" => {
                    let n: usize = v
                        .parse()
                        .map_err(|e| format!("fetch_queue_max: bad count {v:?}: {e}"))?;
                    if n == 0 {
                        return Err(
                            "fetch_queue_max: must be at least 1 (0 would refuse every queued fetch)"
                                .into(),
                        );
                    }
                    cfg.fetch_queue_max = n;
                }
                "verb_max_concurrent" => {
                    let n: usize = v
                        .parse()
                        .map_err(|e| format!("verb_max_concurrent: bad count {v:?}: {e}"))?;
                    if n == 0 {
                        return Err(
                            "verb_max_concurrent: must be at least 1 (0 would stall every verb)"
                                .into(),
                        );
                    }
                    cfg.verb_max_concurrent = n;
                }
                "cas_group" => {
                    if v.is_empty() {
                        return Err("cas_group: empty group name".into());
                    }
                    cfg.cas_group = v.to_string();
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
             lane_bind: 10.42.0.5:4210\nlane_max_uploads: 8\nfetch_max_concurrent: 3\n\
             fetch_queue_max: 8\nverb_max_concurrent: 2\nquota_total_bytes: 100GiB\n\
             quota_owner_default_bytes: 512MiB\nquota_owner: maild=1GiB\n\
             quota_owner: capture=2 GiB\ncas_group: cosmix-blob\nignored_key: whatever\n",
        )
        .unwrap();
        assert_eq!(cfg.root, Some(PathBuf::from("/var/lib/cosmix/blobd-two")));
        assert_eq!(cfg.name.as_deref(), Some("two"));
        assert_eq!(cfg.service_name(), "blobd-two");
        assert_eq!(
            cfg.lane_bind.unwrap().to_string(),
            "10.42.0.5:4210".to_string()
        );
        assert_eq!(cfg.lane_max_uploads, 8);
        assert_eq!(cfg.fetch_max_concurrent, 3);
        assert_eq!(cfg.fetch_queue_max, 8);
        assert_eq!(cfg.verb_max_concurrent, 2);
        assert_eq!(cfg.cas_group, "cosmix-blob");
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
        assert!(Config::parse("lane_max_uploads: 0\n").is_err());
        assert!(Config::parse("lane_max_uploads: lots\n").is_err());
        assert!(Config::parse("fetch_max_concurrent: 0\n").is_err());
        assert!(Config::parse("fetch_max_concurrent: few\n").is_err());
        assert!(Config::parse("fetch_queue_max: 0\n").is_err());
        assert!(Config::parse("fetch_queue_max: lots\n").is_err());
        assert!(Config::parse("verb_max_concurrent: 0\n").is_err());
        assert!(Config::parse("verb_max_concurrent: few\n").is_err());
        assert!(Config::parse("cas_group:\n").is_err());
        assert!(Config::parse("quota_total_bytes: lots\n").is_err());
        assert!(Config::parse("quota_owner: noequals\n").is_err());
        assert!(Config::parse("quota_owner: =5MiB\n").is_err());
        assert!(Config::parse("name:\n").is_err());
    }

    #[test]
    fn empty_text_yields_fetch_defaults() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.fetch_max_concurrent, DEFAULT_FETCH_MAX_CONCURRENT);
        assert_eq!(cfg.fetch_queue_max, DEFAULT_FETCH_QUEUE_MAX);
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
