//! Per-corpus daemon configuration, loaded from a flat `key: value` `.conf.mix` file
//! (the deploy form — the systemd template runs
//! `cosmix-filesd serve -c %E/cosmix/filesd/<corpus>.conf.mix`).
//!
//! One daemon instance serves ONE corpus and registers a distinct Bus service name
//! `filesd-<corpus_id>` (so `cosmix-filesd@notes` and `cosmix-filesd@docs` don't
//! collide on the broker, and webd's future delegated `bus_call` has a stable
//! target). Config keys: `root`, `corpus_id`, `db`, `interval_secs`, `bus_service`.
//!
//! Reserved for a later slice (S6 + grants, deferred): per-corpus capability grants
//! (`props.<action>:<bus_service>.grants[:<corpus_id>]`) — not enforced yet; the
//! trusted on-node `send` path is bounded by the WireGuard /24 trust domain.

use std::collections::HashMap;
use std::path::PathBuf;

use cosmix_files::fsops::Place;

/// The daemon mode a config file selects (`mode:` key, default `corpus`). `fs` runs
/// the generic file-manager `fs.*` capability over a places allowlist instead of the
/// markdown-corpus indexer.
pub fn mode_of(file: &HashMap<String, String>) -> &str {
    file.get("mode").map(String::as_str).unwrap_or("corpus")
}

/// Resolved configuration for an `fs`-mode (file-manager) daemon instance.
#[derive(Debug, Clone)]
pub struct FsConfig {
    pub bus_service: String,
    pub places: Vec<Place>,
    pub trash_root: PathBuf,
    pub delegated_peers: Vec<String>,
}

/// Parse `place:` lines from the raw config text (NOT the flat HashMap — repeated
/// `place:` keys would collide there). Format, `|`-separated, the first three
/// positional then `key=value` options:
///   `place: <id> | <label> | <root> | group=<g> | icon=<i> | writable=<bool>`
/// Parse a comma-separated `allow=`/`deny=` value into validated place-relative
/// glob patterns. Each pattern must be relative (no leading `/`), have no empty /
/// `.` / `..` segments, and use only `[A-Za-z0-9._*-]` plus the `/` separator —
/// so a pattern can never introduce traversal, an absolute path, or the `|`
/// field separator. An explicitly-present option that yields ZERO patterns is a
/// hard error, NOT a silent no-op: `allow=` with nothing after it would
/// otherwise disable the policy and fail OPEN (expose the whole root).
fn parse_patterns(key: &str, v: &str, line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for pat in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if pat.starts_with('/') {
            return Err(format!(
                "place pattern must be relative: {pat:?} in {line:?}"
            ));
        }
        for seg in pat.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err(format!(
                    "invalid segment in place pattern {pat:?} in {line:?}"
                ));
            }
            if !seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'*' | b'-'))
            {
                return Err(format!(
                    "illegal character in place pattern {pat:?} in {line:?}"
                ));
            }
        }
        out.push(pat.to_string());
    }
    if out.is_empty() {
        return Err(format!(
            "{key}= is present but empty in {line:?} (would fail open)"
        ));
    }
    Ok(out)
}

pub fn parse_places(text: &str) -> Result<Vec<Place>, String> {
    let mut places = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.starts_with("place:") {
            continue;
        }
        let body = line["place:".len()..].trim();
        let parts: Vec<&str> = body.split('|').map(str::trim).collect();
        if parts.len() < 3 || parts[0].is_empty() || parts[2].is_empty() {
            return Err(format!(
                "malformed place line (need id|label|root|...): {line:?}"
            ));
        }
        let id = parts[0].to_string();
        if id.contains('/') {
            return Err(format!("place id must not contain '/': {id:?}"));
        }
        let mut place = Place {
            id,
            label: parts[1].to_string(),
            group: "places".to_string(),
            icon: "folder".to_string(),
            root: PathBuf::from(parts[2]),
            writable: false,
            order: places.len(),
            allow: Vec::new(),
            deny: Vec::new(),
        };
        let mut seen_opts: Vec<&str> = Vec::new();
        for opt in &parts[3..] {
            match opt.split_once('=') {
                Some((k, v)) => {
                    let k = k.trim();
                    // A duplicate option (e.g. two `allow=`) is a config error, not
                    // a silent last-wins that could weaken a policy.
                    if seen_opts.contains(&k) {
                        return Err(format!("duplicate place option {k:?} in {line:?}"));
                    }
                    seen_opts.push(k);
                    match k {
                        "group" => place.group = v.trim().to_string(),
                        "icon" => place.icon = v.trim().to_string(),
                        "writable" => place.writable = matches!(v.trim(), "true" | "1" | "yes"),
                        "allow" => place.allow = parse_patterns("allow", v, line)?,
                        "deny" => place.deny = parse_patterns("deny", v, line)?,
                        _ => return Err(format!("unknown place option {k:?} in {line:?}")),
                    }
                }
                None if opt.is_empty() => {}
                None => return Err(format!("place option must be key=value: {opt:?}")),
            }
        }
        places.push(place);
    }
    if places.is_empty() {
        return Err("fs-mode config has no `place:` lines".to_string());
    }
    Ok(places)
}

/// Resolve an `fs`-mode config from the raw config text plus an optional CLI
/// `bus_service` override. Requires `trash_root` and at least one `place:`.
pub fn resolve_fs(text: &str, bus_service: Option<String>) -> Result<FsConfig, String> {
    let f = parse(text);
    let bus_service = bus_service
        .or_else(|| f.get("bus_service").cloned())
        .unwrap_or_else(|| "filesd-fs".to_string());
    let trash_root = f
        .get("trash_root")
        .map(PathBuf::from)
        .ok_or("config: missing 'trash_root'")?;
    let places = parse_places(text)?;
    // Same delegated_peers semantics as the corpus config: explicit-empty disables.
    let delegated_peers = match f.get("delegated_peers") {
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        None => vec!["webd".to_string()],
    };
    Ok(FsConfig {
        bus_service,
        places,
        trash_root,
        delegated_peers,
    })
}

/// Resolved configuration for one corpus daemon instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusConfig {
    pub root: PathBuf,
    pub corpus_id: String,
    pub db: PathBuf,
    pub interval_secs: u64,
    /// Bus service name to register as (default `filesd-<corpus_id>`).
    pub bus_service: String,
    /// Peers (`cmd.from`) allowed to use the `$cosmix_delegation` front-door
    /// path (default `["webd"]`). A delegated call from any other peer fails
    /// closed; these peers must always present an envelope (defence-in-depth).
    pub delegated_peers: Vec<String>,
}

/// Parse a flat `key: value` config — `#` comments and blank lines are ignored,
/// the key is split on the first `:`, both sides trimmed.
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            m.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    m
}

/// Resolve the config from an optional parsed file map plus CLI overrides (CLI wins).
/// `root`, `corpus_id`, and `db` are required (from either source); `interval_secs`
/// defaults to 900 and `bus_service` to `filesd-<corpus_id>`.
pub fn resolve(
    file: Option<HashMap<String, String>>,
    root: Option<PathBuf>,
    corpus: Option<String>,
    db: Option<PathBuf>,
    interval_secs: Option<u64>,
    bus_service: Option<String>,
) -> Result<CorpusConfig, String> {
    let f = file.unwrap_or_default();
    let root = root
        .or_else(|| f.get("root").map(PathBuf::from))
        .ok_or("config: missing 'root'")?;
    let corpus_id = corpus
        .or_else(|| f.get("corpus_id").cloned())
        .ok_or("config: missing 'corpus_id'")?;
    let db = db
        .or_else(|| f.get("db").map(PathBuf::from))
        .ok_or("config: missing 'db'")?;
    let interval_secs = interval_secs
        .or_else(|| f.get("interval_secs").and_then(|s| s.parse().ok()))
        .unwrap_or(900);
    let bus_service = bus_service
        .or_else(|| f.get("bus_service").cloned())
        .unwrap_or_else(|| format!("filesd-{corpus_id}"));
    // An explicitly PRESENT key wins even if it parses empty (→ delegation OFF,
    // so an operator can disable the front door); only an ABSENT key defaults to
    // ["webd"]. (Diverges from the earlier collapse that silently re-enabled the
    // default on an empty value.)
    let delegated_peers = match f.get("delegated_peers") {
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        None => vec!["webd".to_string()],
    };
    Ok(CorpusConfig {
        root,
        corpus_id,
        db,
        interval_secs,
        bus_service,
        delegated_peers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ignores_comments_and_blanks() {
        let m = parse("# a corpus\n\nroot: /srv/notes\ncorpus_id: notes\n  db : /var/x.db \n");
        assert_eq!(m.get("root").unwrap(), "/srv/notes");
        assert_eq!(m.get("corpus_id").unwrap(), "notes");
        assert_eq!(m.get("db").unwrap(), "/var/x.db"); // trimmed both sides
        assert!(!m.contains_key("# a corpus"));
    }

    #[test]
    fn resolve_defaults_and_required() {
        let f = parse("root: /srv/notes\ncorpus_id: notes\ndb: /var/notes.db\n");
        let c = resolve(Some(f), None, None, None, None, None).unwrap();
        assert_eq!(c.corpus_id, "notes");
        assert_eq!(c.interval_secs, 900); // default
        assert_eq!(c.bus_service, "filesd-notes"); // default = filesd-<corpus_id>
        assert_eq!(c.delegated_peers, vec!["webd".to_string()]); // default
        assert_eq!(c.db, PathBuf::from("/var/notes.db"));
        // delegated_peers parses comma-separated, trims, drops empties.
        let f2 = parse("root: /r\ncorpus_id: n\ndb: /d\ndelegated_peers: webd, webd2 ,\n");
        let c2 = resolve(Some(f2), None, None, None, None, None).unwrap();
        assert_eq!(
            c2.delegated_peers,
            vec!["webd".to_string(), "webd2".to_string()]
        );
        // An explicitly EMPTY delegated_peers disables the delegated front door
        // (NOT silently re-defaulted to ["webd"]).
        let f3 = parse("root: /r\ncorpus_id: n\ndb: /d\ndelegated_peers:  \n");
        let c3 = resolve(Some(f3), None, None, None, None, None).unwrap();
        assert!(
            c3.delegated_peers.is_empty(),
            "explicit-empty disables delegation"
        );
        // Missing required → Err.
        assert!(resolve(Some(parse("corpus_id: x\n")), None, None, None, None, None).is_err());
    }

    #[test]
    fn fs_mode_detect_and_resolve() {
        let text = "mode: fs\nbus_service: filesd-fs\ntrash_root: /srv/fm/.Trash\n\
                    place: home | Home | /srv/fm/home | group=places | icon=home | writable=true\n\
                    place: media | Media | /srv/media | writable=false\n";
        let f = parse(text);
        assert_eq!(mode_of(&f), "fs");
        let cfg = resolve_fs(text, None).unwrap();
        assert_eq!(cfg.bus_service, "filesd-fs");
        assert_eq!(cfg.trash_root, PathBuf::from("/srv/fm/.Trash"));
        assert_eq!(cfg.places.len(), 2);
        assert_eq!(cfg.places[0].id, "home");
        assert_eq!(cfg.places[0].label, "Home");
        assert_eq!(cfg.places[0].icon, "home");
        assert!(cfg.places[0].writable);
        assert_eq!(cfg.places[0].order, 0);
        // defaults: media has no group/icon → defaults, writable=false explicit
        assert_eq!(cfg.places[1].group, "places");
        assert_eq!(cfg.places[1].icon, "folder");
        assert!(!cfg.places[1].writable);
        assert_eq!(cfg.delegated_peers, vec!["webd".to_string()]);
    }

    #[test]
    fn fs_mode_default_mode_is_corpus() {
        let f = parse("root: /r\ncorpus_id: n\ndb: /d\n");
        assert_eq!(mode_of(&f), "corpus");
    }

    #[test]
    fn place_allow_deny_parsing() {
        let text = "mode: fs\ntrash_root: /t\n\
            place: sshm | SSH | /home/user/.ssh | writable=true | \
            allow=config,hosts/*,keys/*.pub | deny=.git\n";
        let cfg = resolve_fs(text, None).unwrap();
        let p = &cfg.places[0];
        assert_eq!(p.allow, vec!["config", "hosts/*", "keys/*.pub"]);
        assert_eq!(p.deny, vec![".git"]);
        // A place with no allow/deny keeps empty policy (unchanged behaviour).
        let plain = "mode: fs\ntrash_root: /t\nplace: home | Home | /h | writable=true\n";
        assert!(resolve_fs(plain, None).unwrap().places[0].allow.is_empty());
    }

    #[test]
    fn place_pattern_validation_rejects_traversal_and_junk() {
        for bad in [
            "allow=/abs",      // absolute
            "allow=../escape", // traversal
            "allow=a/../b",    // interior traversal
            "allow=a b",       // whitespace / illegal char
            "deny=a;rm",       // illegal char
            "allow=",          // fail-open: present but empty
            "allow=,",         // fail-open: only separators
            "deny=",           // present but empty
        ] {
            let text =
                format!("mode: fs\ntrash_root: /t\nplace: p | P | /r | writable=true | {bad}\n");
            assert!(resolve_fs(&text, None).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn duplicate_place_option_rejected() {
        let text = "mode: fs\ntrash_root: /t\n\
            place: p | P | /r | writable=true | allow=config | allow=hosts/*\n";
        assert!(
            resolve_fs(text, None).is_err(),
            "duplicate allow= must be rejected"
        );
    }

    #[test]
    fn fs_mode_rejects_bad_config() {
        // no places
        assert!(resolve_fs("mode: fs\ntrash_root: /t\n", None).is_err());
        // no trash_root
        assert!(resolve_fs("mode: fs\nplace: a | A | /a\n", None).is_err());
        // malformed place (too few fields)
        assert!(parse_places("place: a | A\n").is_err());
        // id with a slash
        assert!(parse_places("place: a/b | A | /a\n").is_err());
        // unknown option
        assert!(parse_places("place: a | A | /a | bogus=1\n").is_err());
    }

    #[test]
    fn cli_overrides_file_and_explicit_bus_service() {
        let f = parse(
            "root: /file/root\ncorpus_id: notes\ndb: /file/db\ninterval_secs: 60\nbus_service: filesd-custom\n",
        );
        let c = resolve(
            Some(f),
            Some(PathBuf::from("/cli/root")), // overrides file
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(c.root, PathBuf::from("/cli/root"));
        assert_eq!(c.interval_secs, 60); // from file
        assert_eq!(c.bus_service, "filesd-custom"); // explicit in file
    }
}
