//! Configuration for cosmix-jmap.

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

pub use cosmix_config::node::{TlsConfig, TlsIdentityConfig};

/// A listen-address spec that accepts either a single string or a list.
///
/// This lets `smtp_inbound = "192.0.2.4:25"` and
/// `smtp_inbound = ["192.0.2.4:25", "198.51.100.202:25"]` both parse —
/// existing single-string configs keep working while operators who
/// need multi-homed binds (e.g. WG IP + LAN IP for real-world testing)
/// can list them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ListenSpec {
    Single(String),
    Multiple(Vec<String>),
}

impl ListenSpec {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::Single(s) => vec![s.clone()],
            Self::Multiple(v) => v.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    /// JMAP HTTP listen address
    pub listen: String,
    /// Public base URL for JMAP
    pub base_url: String,
    /// SQLite database file path
    pub database_path: String,
    /// Blob storage directory
    pub blob_dir: String,
    /// MDS (Mailbox Data Store) directory. The MailStore adapter
    /// opens / creates an `SqliteCasMds` rooted here. Phase A wires
    /// the store into AppState; later phases migrate JMAP and SMTP
    /// reads/writes onto it. Defaults to `var/jmap/mds`.
    pub mds_dir: String,
    /// Server hostname (used in SMTP EHLO and DKIM)
    pub hostname: String,

    // SMTP settings
    /// SMTP inbound listen address(es) (port 25). Accepts a single
    /// string or an array of strings. None to disable.
    pub smtp_inbound: Option<ListenSpec>,
    /// Inbound bind addresses (a subset of `smtp_inbound`) that REQUIRE
    /// an active STARTTLS upgrade before accepting mail — a plaintext
    /// `MAIL FROM` on a listed bind is rejected `530`. Empty (the
    /// default) = opportunistic STARTTLS on every inbound bind (a
    /// general-purpose MX must accept plaintext or it breaks delivery
    /// from non-TLS senders). Use this to require TLS only on a *public*
    /// bind — e.g. `["203.0.113.5:25"]` — while the mesh bind stays
    /// opportunistic. Entries must match `smtp_inbound` bind strings
    /// exactly (the listener compares against the accepted local addr).
    #[serde(default)]
    pub require_starttls_inbound: Vec<String>,
    /// SMTPS submission listen address(es) (port 465, implicit TLS).
    /// Accepts a single string or an array of strings. None to disable.
    pub smtp_smtps: Option<ListenSpec>,
    /// Local source address(es) to bind for OUTBOUND SMTP delivery
    /// connections, e.g. `["2403:580a:5e99::100"]`. At most one entry
    /// per family applies: dialling an IPv6 MX binds the v6 entry (if
    /// any), an IPv4 MX the v4 entry. Empty (default) = kernel source
    /// selection. Use when the SPF-aligned address is not what the
    /// default route would pick (gco: v6 default egress is the eth0
    /// EUI-64, but the MX AAAA / SPF address is eth1's ::100).
    #[serde(default)]
    pub smtp_outbound_bind: Vec<String>,
    /// Maximum message size in bytes
    pub max_message_size: Option<usize>,
    /// DKIM selector (e.g., "default"). Legacy single-key shape;
    /// stays as the unsigned-domain fallback when `[[dkim.domain]]`
    /// rows are also present (multi-vhost umbrella Phase 1).
    pub dkim_selector: Option<String>,
    /// Path to DKIM private key (PEM). Legacy single-key shape; see
    /// `dkim_selector` doc above.
    pub dkim_private_key: Option<String>,
    /// Per-domain DKIM signing rows (multi-vhost umbrella Phase 1).
    /// Each `[[dkim.domain]]` row maps to a
    /// `cosmix_maild_auth::DkimSignerConfig`; `MailAuthSigner` is
    /// built from these at startup. When empty, the legacy single-
    /// key path runs verbatim. See
    /// `_doc/planned/maild-multi-vhost-phase1.md`.
    #[serde(default)]
    pub dkim: DkimSubsection,
    /// Path to TLS certificate (PEM). Legacy single-identity shape.
    /// Ignored when `tls.identity` is non-empty; otherwise
    /// `resolve_tls_identities()` collapses this pair into a single
    /// identity using `hostname` as the SNI label.
    pub tls_cert: Option<String>,
    /// Path to TLS private key (PEM). Paired with `tls_cert`; see
    /// the field doc there for the legacy-collapse semantics.
    pub tls_key: Option<String>,
    /// Multi-identity TLS (SNI-routed). When `tls.identity` is non-
    /// empty, the resolver consumes it directly and the legacy
    /// `tls_cert` / `tls_key` pair is ignored. See
    /// `_doc/planned/maild-sni-internal-ca.md` and
    /// `resolve_tls_identities()`.
    /// Bus sender names (`cmd.from`) permitted to **write** the
    /// `maild.retention` config namespace — i.e. to arm/disarm automatic
    /// Junk/Trash deletion. Reads stay open to every WG peer; only this
    /// allowlist can flip a window non-zero or clear `dry_run`. **Empty
    /// (the default) ⇒ no remote arming at all** — retention cannot be
    /// turned on over Bus until an operator is named here. Mirrors the
    /// `webd.listeners` operator-allowlist pattern.
    #[serde(default)]
    pub retention_operators: Vec<String>,

    /// Optional Bus sender allowlist for `maild.bayesian.rebuild`. This is
    /// deliberately inverse to `retention_operators`: **empty (the default)
    /// means every Bus peer may rebuild** (the substrate's agentic-first
    /// posture); a non-empty list restricts rebuilds to exact `cmd.from`
    /// matches. Bayesian reads remain open in both states.
    #[serde(default)]
    pub bayesian_rebuild_operators: Vec<String>,

    /// Bus senders (`cmd.from`) authorised for the `maild.vtoken.*` management
    /// verbs (add/list/rotate/disable/lookup). Token management mints the
    /// publish capability, so EVERY vtoken verb is gated on this allowlist (not
    /// just the mutating ones). **Empty (the default) ⇒ nobody** — vtoken
    /// management is unavailable over Bus until an operator is named. Same shape
    /// as `retention_operators`.
    #[serde(default)]
    pub vtoken_operators: Vec<String>,

    /// Bus peers (`cmd.from`) permitted to make DELEGATED `maild.vtoken.*`
    /// calls — i.e. webd, relaying a logged-in vhost-admin actor via the
    /// `$cosmix_delegation` envelope (vtoken C2). DISTINCT from
    /// `vtoken_operators`: an operator is a trusted CLI acting globally; a
    /// delegated peer is a front-door whose authority is bounded to the
    /// envelope's actor and that actor's own domain. **Empty (the default) ⇒
    /// no delegation at all** — a delegated call is refused until a peer is
    /// named here, and the delegated path never falls back to operator auth.
    #[serde(default)]
    pub vtoken_delegated_peers: Vec<String>,

    #[serde(default)]
    pub tls: TlsConfig,
    /// Filesystem root for substrate-declared TLS identities. The
    /// Phase 5a-commit-2 `maild.tls.reload` verb resolves
    /// `maild.domains.tls_identity_name = "<name>"` rows to PEM paths
    /// at `<tls_key_root>/<name>.cert.pem` +
    /// `<tls_key_root>/<name>.key.pem`. Unset = default
    /// (`/var/lib/cosmix/maild/tls`). `[[tls.identity]]` config rows
    /// ignore this and read their `cert` / `key` paths directly.
    pub tls_key_root: Option<PathBuf>,

    // IMAP settings (Phase 1 — listener + auth).
    /// IMAPS listen address(es) (port 993, implicit TLS). Accepts a
    /// single string or an array. None to disable. TLS material is
    /// read from `tls_cert`/`tls_key`; both must be set for IMAPS.
    pub imap_imaps: Option<ListenSpec>,
    /// Max literal size on APPEND (64 MiB default — Phase 4 only).
    pub imap_max_literal_bytes: Option<u64>,
    /// IDLE `* OK Still here` keepalive interval (Phase 5 use; 120 s
    /// default).
    pub imap_idle_status_interval_secs: Option<u64>,
    /// Pre-auth idle timeout (180 s default; tighten to 30 s once the
    /// real-client matrix is green).
    pub imap_pre_auth_timeout_secs: Option<u64>,
    /// Per-connection auth failure cap (default 3).
    pub imap_max_auth_failures: Option<u32>,
    /// Pre-auth bad-command cap (default 3).
    pub imap_max_bad_commands_pre_auth: Option<u32>,
    /// Post-auth bad-command cap (default 20).
    pub imap_max_bad_commands_post_auth: Option<u32>,
    /// Per-account concurrent connection cap (default 20).
    pub imap_max_concurrent_per_account: Option<u32>,
    /// Operator override of the advertised capability list. When
    /// `None`, the Phase 1 initial subset is used.
    pub imap_advertise_capabilities: Option<Vec<String>>,

    // Filter settings
    /// Path to a Mix script for inbound mail routing (optional).
    /// The script receives $FROM (SMTP envelope sender), $TO, $SUBJECT,
    /// $HEADER_FROM (From: header display-name + address, survives a null
    /// envelope sender — i.e. bounces), $SPAM_VERDICT, $SPAM_SCORE as globals
    /// and should `return` a mailbox name (e.g. "Inbox", "Work").
    pub inbound_filter: Option<String>,

    // Spam filter settings
    /// Enable spam filtering (default: true)
    pub spam_enabled: Option<bool>,
    /// Spamlite per-user database directory
    pub spam_db_dir: Option<String>,
    /// Baseline spamlite database for new accounts
    pub spam_baseline_db: Option<String>,

    /// EXPERIMENTAL (default off): use the corpus's observed spam base rate as
    /// the Bayesian Robinson centre, instead of the fixed 0.5 "an unseen word is
    /// a coin flip" assumption. Shrunk toward 0.5 by `spam_base_rate_pseudocount`
    /// and clamped to `[spam_base_rate_min, spam_base_rate_max]`.
    ///
    /// Exists to be measured, not to be switched on: it is the lever for the
    /// thin-corpus false-positive class, and it carries its own risk (a
    /// spam-heavy mailbox could start scoring novel HAM vocabulary as spam).
    /// Shadow-evaluate on mrn first. See
    /// `~/.cmctl/_doc/2026-07-14-bayes-base-rate-prior.md`.
    pub spam_base_rate_prior: Option<bool>,
    /// Shrinkage toward 0.5, in pseudo-messages. Default 20.
    pub spam_base_rate_pseudocount: Option<f32>,
    /// Lower clamp on the derived centre. Default 0.2.
    pub spam_base_rate_min: Option<f32>,
    /// Upper clamp on the derived centre. Default 0.8.
    pub spam_base_rate_max: Option<f32>,

    // Rule engine settings
    /// Path to a TOML rule pack on disk. When set, the rule engine
    /// loads from this path at startup and `RuleEngine::reload()`
    /// re-reads the same file. When unset, the default v1.0 pack
    /// embedded via `include_str!` is used and reload is a no-op.
    pub rules_pack_path: Option<String>,
    /// Background-flush cadence for the persistent rule-stats store.
    /// Defaults to 60s (see `rule_stats_flush_interval_secs()` below).
    /// Smaller values shorten the *process-restart* loss window
    /// (panic, SIGKILL, `systemctl restart`) at the cost of more
    /// SQLite write activity; larger values amortise writes. Host or
    /// power loss is bounded by the WAL/checkpoint cadence, not this
    /// setting — see the `rule_stats_store` module docstring for the
    /// full durability matrix.
    pub rule_stats_flush_interval_secs: Option<u64>,
    /// Root directory for rule-engine persistent state (currently
    /// just `stats.db`). Defaults to `<var>/maild/rules/` — see
    /// `rule_stats_dir()`. Kept disjoint from `spam_db_dir` so
    /// Bayesian per-account dbs and global rule-engine state can be
    /// re-pathed independently.
    pub rule_stats_dir: Option<String>,
}

/// `[dkim]` config subsection — wraps the `[[dkim.domain]]` array of
/// tables. Lives in its own struct so the wire shape stays
/// `[[dkim.domain]] { … }` (TOML array-of-tables syntax) rather than
/// requiring a top-level `dkim_domains = [...]` list.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct DkimSubsection {
    /// One row per (domain, selector). The `MailAuthSigner` buckets
    /// rows by domain and walks parent labels at sign time.
    #[serde(rename = "domain", default)]
    pub domains: Vec<DkimDomainConfig>,
    /// Filesystem root for substrate-managed signer PEMs. The Phase 4
    /// verbs (`maild.dkim.{generate,rotate,retire}`) write keys here
    /// as `<key_root>/<domain>/<selector>.pem`. Unset = default
    /// (`/var/lib/cosmix/maild/dkim`). Operator-managed
    /// `[[dkim.domain]]` rows ignore this and read their `key_path`
    /// directly.
    pub key_root: Option<PathBuf>,
}

/// One `[[dkim.domain]]` TOML row — the operator-facing shape that
/// converts into a `cosmix_maild_auth::DkimSignerConfig` at startup.
#[derive(Debug, Deserialize, Clone)]
pub struct DkimDomainConfig {
    /// Organisational / mail-host domain — what gets stamped as `d=`.
    /// Lowercased + ASCII-validated; non-ASCII rejected (DKIM keys
    /// are looked up by exact byte match in DNS, and operators don't
    /// IDN their selectors in practice — surface the mismatch at
    /// config load rather than at verifier time).
    pub domain: String,
    /// `s=` tag (also the on-disk PEM filename: `<selector>.pem`).
    pub selector: String,
    /// One of `"rsa-sha256"` or `"ed25519-sha256"`.
    pub algorithm: String,
    /// Path to the PEM file produced by `mix dkim-keygen.mix`.
    /// Read into memory at startup; the bytes are validated against
    /// the declared `algorithm` (full key construction, not just
    /// PEM-envelope decode) so a bad key fails the daemon before it
    /// can deliver mail.
    pub key_path: PathBuf,
    /// `(header-canon, body-canon)`. Each entry is `"simple"` or
    /// `"relaxed"`. Defaults to `("relaxed", "relaxed")` per RFC 6376
    /// §3.4.4 common practice.
    pub canonicalization: Option<(String, String)>,
    /// Headers to cover. Entries prefixed with `"!"` are oversigned
    /// (the header name appears twice in `h=`, defeating later
    /// `From:`-prepend attacks). Defaults to
    /// `cosmix_maild_auth::default_signed_headers()`.
    pub headers: Option<Vec<String>>,
    /// Multiple rows per domain are legal (zero-downtime key rotation
    /// holds two selectors loaded). At most one row per domain may
    /// have `active_for_signing = true` — the daemon hard-fails at
    /// load otherwise, because `MailAuthSigner::lookup_active` would
    /// pick the first active entry in TOML insertion order and the
    /// chosen key would silently depend on ordering.
    #[serde(default = "default_true")]
    pub active_for_signing: bool,
    /// `l=` body-length tag. Off by default — enabling allows a
    /// downstream relay to append content without breaking the
    /// signature, a known DKIM weakening.
    #[serde(default)]
    pub allow_body_length_tag: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8088".into(),
            base_url: "http://127.0.0.1:8088".into(),
            database_path: default_db_path(),
            blob_dir: default_blob_dir(),
            mds_dir: default_mds_dir(),
            hostname: "localhost".into(),
            inbound_filter: None,
            smtp_inbound: Some(ListenSpec::Single("0.0.0.0:2525".into())),
            require_starttls_inbound: Vec::new(),
            smtp_smtps: None,
            smtp_outbound_bind: Vec::new(),
            max_message_size: None,
            dkim_selector: None,
            dkim_private_key: None,
            dkim: DkimSubsection::default(),
            tls_cert: None,
            tls_key: None,
            retention_operators: Vec::new(),
            bayesian_rebuild_operators: Vec::new(),
            vtoken_operators: Vec::new(),
            vtoken_delegated_peers: Vec::new(),
            tls: TlsConfig::default(),
            tls_key_root: None,
            imap_imaps: None,
            imap_max_literal_bytes: None,
            imap_idle_status_interval_secs: None,
            imap_pre_auth_timeout_secs: None,
            imap_max_auth_failures: None,
            imap_max_bad_commands_pre_auth: None,
            imap_max_bad_commands_post_auth: None,
            imap_max_concurrent_per_account: None,
            imap_advertise_capabilities: None,
            spam_enabled: Some(true),
            spam_db_dir: None,
            spam_base_rate_prior: None,
            spam_base_rate_pseudocount: None,
            spam_base_rate_min: None,
            spam_base_rate_max: None,
            spam_baseline_db: None,
            rules_pack_path: None,
            rule_stats_flush_interval_secs: None,
            rule_stats_dir: None,
        }
    }
}

/// Substrate-managed DKIM PEM directory. Verbs and the rebuild loop
/// both write/read here when `[dkim] key_root = …` is unset.
pub fn default_dkim_key_root() -> PathBuf {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var).join("maild/dkim")
}

/// Substrate-managed TLS PEM directory. Phase 5a-commit-2's
/// `maild.tls.reload` verb resolves substrate-declared identities to
/// `<tls_key_root>/<name>.{cert,key}.pem` here when `tls_key_root = …`
/// is unset. `[[tls.identity]]` config rows are not affected.
pub fn default_tls_key_root() -> PathBuf {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var).join("maild/tls")
}

fn default_db_path() -> String {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
        .join("jmap/mail.db")
        .to_string_lossy()
        .into_owned()
}

fn default_blob_dir() -> String {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
        .join("jmap/blobs")
        .to_string_lossy()
        .into_owned()
}

fn default_mds_dir() -> String {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
        .join("jmap/mds")
        .to_string_lossy()
        .into_owned()
}

fn default_spam_dir() -> String {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var)
        .join("jmap/spamlite")
        .to_string_lossy()
        .into_owned()
}

/// Default root for rule-engine state. Rule stats are global (not
/// per-account) and are NOT Bayesian training data, so they own a
/// dedicated `maild/rules/` root rather than piggy-backing on
/// `spam_db_dir` (which is Bayesian-per-account territory and may be
/// re-pathed independently).
fn default_rules_state_dir() -> PathBuf {
    cosmix_config::cosmix_path(cosmix_config::CosmixDir::Var).join("maild/rules")
}

impl From<&cosmix_config::node::NodeConfig> for Config {
    fn from(nc: &cosmix_config::node::NodeConfig) -> Self {
        let m = &nc.maild;
        Self {
            listen: nc.jmap_listen(),
            base_url: nc.jmap_upstream(),
            database_path: m.database.clone(),
            blob_dir: m.blob_dir.clone(),
            mds_dir: m.mds_dir.clone(),
            hostname: m.hostname.clone(),
            inbound_filter: None,
            smtp_inbound: Some(ListenSpec::Single(nc.smtp_inbound_listen())),
            require_starttls_inbound: Vec::new(),
            smtp_smtps: Some(ListenSpec::Single(nc.smtps_listen())),
            smtp_outbound_bind: Vec::new(),
            max_message_size: None,
            dkim_selector: None,
            dkim_private_key: None,
            dkim: DkimSubsection::default(),
            tls_cert: m.tls_cert.clone(),
            tls_key: m.tls_key.clone(),
            tls: m.tls.clone(),
            // The base-rate prior is an EXPERIMENT: it is set from
            // `config.conf.mix` (the path every mesh node actually boots from,
            // via `-c`), not from the NodeConfig fallback. `None` here is the
            // safe posture — off — and matches the retention precedent below:
            // a schema that doesn't carry a field yet must not silently invent
            // one, least of all one that changes how mail is scored.
            spam_base_rate_prior: None,
            spam_base_rate_pseudocount: None,
            spam_base_rate_min: None,
            spam_base_rate_max: None,
            // The `NodeConfig` schema doesn't carry a retention operator
            // allowlist yet; default to empty (no remote arming) — the
            // safe posture. A mesh node arms retention by adding the
            // field once NodeConfig grows it (retention P2).
            retention_operators: Vec::new(),
            bayesian_rebuild_operators: Vec::new(),
            vtoken_operators: Vec::new(),
            vtoken_delegated_peers: Vec::new(),
            tls_key_root: None,
            imap_imaps: None,
            imap_max_literal_bytes: None,
            imap_idle_status_interval_secs: None,
            imap_pre_auth_timeout_secs: None,
            imap_max_auth_failures: None,
            imap_max_bad_commands_pre_auth: None,
            imap_max_bad_commands_post_auth: None,
            imap_max_concurrent_per_account: None,
            imap_advertise_capabilities: None,
            spam_enabled: Some(m.spam_enabled),
            spam_db_dir: m.spam_db_dir.clone(),
            spam_baseline_db: None,
            rules_pack_path: None,
            rule_stats_flush_interval_secs: None,
            rule_stats_dir: None,
        }
    }
}

impl Config {
    /// Load from an explicit `--config <path>` (`.conf.mix`).
    pub fn load(path: &str) -> Result<Self> {
        let config: Config = cosmix_config::load_conf_mix_path(std::path::Path::new(path))?;
        config
            .validate_tls()
            .map_err(|e| anyhow::anyhow!("invalid tls.identity in config {path}: {e}"))?;
        Ok(config)
    }

    /// Reject `[[tls.identity]]` rows whose required string fields
    /// are empty/whitespace-only.
    ///
    /// Required-field *omission* is already a hard parse error
    /// (`TlsIdentityConfig` has no struct-level `serde(default)`),
    /// but TOML permits an explicit `""` literal for any string. A
    /// blank `server_name` / `cert` / `key` would parse, slip past
    /// the legacy-collapse short-circuit in
    /// `resolve_tls_identities()` (it triggers on
    /// `!self.tls.identity.is_empty()`, not on the row's content),
    /// and ultimately hand an unservable identity to the step-2 SNI
    /// resolver. Catching it at load time turns a runtime
    /// "no cert for this connection" surprise into a startup error
    /// the operator can actually fix.
    pub fn validate_tls(&self) -> Result<(), String> {
        for (i, ident) in self.tls.identity.iter().enumerate() {
            if ident.server_name.trim().is_empty() {
                return Err(format!("identity[{i}].server_name is empty"));
            }
            if ident.cert.trim().is_empty() {
                return Err(format!(
                    "identity[{i}] (server_name={:?}): cert is empty",
                    ident.server_name
                ));
            }
            if ident.key.trim().is_empty() {
                return Err(format!(
                    "identity[{i}] (server_name={:?}): key is empty",
                    ident.server_name
                ));
            }
        }
        Ok(())
    }

    /// Best-effort load of an optional config file.
    ///
    /// Distinguishes three outcomes the caller cares about:
    ///
    /// * `Ok(Some(cfg))` — file read and parsed successfully.
    /// * `Ok(None)` — file is absent (NotFound). The caller should
    ///   fall through to the next search location. This is the
    ///   only IO error that means "the file is not there"; all
    ///   others (including `PermissionDenied`) indicate the file
    ///   exists but cannot be used, and silently substituting a
    ///   different config would route the caller to a *different
    ///   database* than the daemon — a subtle "looks empty / works
    ///   on the wrong DB" failure mode that wastes operator time.
    /// * `Err(_)` — file exists but can't be loaded: permission
    ///   denied, IO failure, or `.conf.mix` syntax / deserialization
    ///   error. Hard-fail so the operator sees the actual problem.
    pub fn try_load_optional(path: &std::path::Path) -> Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to read config {}: {e}",
                    path.display()
                ));
            }
        };
        // Parse the strict-data `.conf.mix`. A found-but-broken config is
        // a hard error, never a silent fall-through to a different file.
        let config: Config = cosmix_config::from_conf_mix_str(&contents)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))?;
        config.validate_tls().map_err(|e| {
            anyhow::anyhow!("invalid tls.identity in config {}: {e}", path.display())
        })?;
        Ok(Some(config))
    }

    /// XDG user config path: `~/.config/cosmix/jmap.conf.mix`.
    pub fn config_path() -> PathBuf {
        cosmix_config::cosmix_path(cosmix_config::CosmixDir::Etc).join("jmap.conf.mix")
    }

    /// System-wide config path: `/etc/cosmix/maild/config.conf.mix`.
    ///
    /// This is the location the systemd unit reads from on a real
    /// deployment. Searched ahead of the XDG user path so a non-root
    /// caller (e.g. `markc` running `cosmix-maild account list`)
    /// resolves to the daemon's actual config — same database, same
    /// MDS, same account roster — without `--config` or `sudo -u`.
    /// The XDG user path remains a fallback for per-user / dev
    /// installs where no system config exists.
    pub fn system_config_path() -> PathBuf {
        PathBuf::from("/etc/cosmix/maild/config.conf.mix")
    }

    pub fn spam_db_dir(&self) -> String {
        self.spam_db_dir.clone().unwrap_or_else(default_spam_dir)
    }

    /// Effective cadence (seconds) for the persistent rule-stats
    /// flush task. Defaults to 60s per `_doc/planned/maild-rules-phase2.md`
    /// (≤60s of counter increments lost on a clean process restart;
    /// see `rule_stats_store` module docstring for the host/power-loss
    /// case, which is unbounded by design). Zero is rejected at
    /// validation time elsewhere; the runtime treats `0` here as
    /// "fall back to default" to avoid a busy-loop if a hand-edited
    /// config slips past validation.
    pub fn rule_stats_flush_interval_secs(&self) -> u64 {
        match self.rule_stats_flush_interval_secs {
            Some(0) | None => 60,
            Some(n) => n,
        }
    }

    /// Effective directory for rule-engine persistent state. Defaults
    /// to `<var>/maild/rules/`. Distinct from `spam_db_dir` because
    /// rule stats are global rule-engine counters, not Bayesian
    /// per-account training data — the two are migrated and pathed
    /// independently.
    pub fn rule_stats_dir(&self) -> PathBuf {
        match &self.rule_stats_dir {
            Some(s) => PathBuf::from(s),
            None => default_rules_state_dir(),
        }
    }

    /// Effective TLS identity list for the SNI resolver.
    ///
    /// Precondition: `validate_tls()` has been called (or the
    /// `Config` came from `Config::load` / `try_load_optional`,
    /// which call it for you). Direct in-memory construction in
    /// tests bypasses validation; callers building a `Config`
    /// without going through `load` must validate themselves before
    /// trusting the returned vec.
    ///
    /// Precedence (per `_doc/planned/maild-sni-internal-ca.md`):
    ///
    /// 1. If `tls.identity` is non-empty, return it verbatim — the
    ///    operator has opted in to multi-identity TLS and the
    ///    legacy `tls_cert` / `tls_key` pair is ignored.
    /// 2. Otherwise, if both `tls_cert` and `tls_key` are set,
    ///    collapse into a single-identity list with
    ///    `server_name = hostname`, `default = true`,
    ///    `no_sni_fallback = true`. This is the backward-compat
    ///    shape — the resolver serves the same cert regardless of
    ///    SNI (Path A in the plan), preserving existing
    ///    single-cert deployments byte-identically.
    /// 3. Otherwise, return an empty list — TLS is disabled.
    ///
    /// Path A operators do not need to touch their config when
    /// Phase 1 lands; this method makes the legacy shape look like
    /// the new shape to downstream callers.
    pub fn resolve_tls_identities(&self) -> Vec<TlsIdentityConfig> {
        if !self.tls.identity.is_empty() {
            return self.tls.identity.clone();
        }
        match (&self.tls_cert, &self.tls_key) {
            (Some(cert), Some(key)) => vec![TlsIdentityConfig {
                server_name: self.hostname.clone(),
                cert: cert.clone(),
                key: key.clone(),
                default: true,
                no_sni_fallback: true,
            }],
            _ => Vec::new(),
        }
    }

    /// Strict-SNI mode toggle (mirrors `TlsConfig::strict_sni`).
    pub fn strict_sni(&self) -> bool {
        self.tls.strict_sni
    }

    /// Convert `[[dkim.domain]]` rows into the
    /// `cosmix_maild_auth::DkimSignerConfig` shape the signer
    /// consumes. Reads PEM files, validates each against the declared
    /// algorithm via `MailAuthSigner::validate_pem` (full key
    /// construction — PEM-envelope decode alone is insufficient),
    /// and enforces "at most one active row per domain" so the
    /// active-selector lookup is deterministic.
    ///
    /// Returns `Ok(vec![])` when the operator hasn't configured any
    /// rows — runtime then leaves `mail_auth_signer = None` and the
    /// legacy single-key path runs verbatim.
    pub fn resolve_dkim_signer_configs(&self) -> Result<Vec<cosmix_maild_auth::DkimSignerConfig>> {
        use cosmix_maild_auth::{
            Canon, DkimAlgorithm, DkimSignerConfig, Domain, HeaderSpec, MailAuthSigner,
            default_signed_headers,
        };

        let mut out: Vec<DkimSignerConfig> = Vec::with_capacity(self.dkim.domains.len());
        let mut active_seen: std::collections::HashMap<Domain, usize> =
            std::collections::HashMap::new();

        for (i, row) in self.dkim.domains.iter().enumerate() {
            let domain_str = row.domain.trim().to_ascii_lowercase();
            if domain_str.is_empty() {
                anyhow::bail!("[[dkim.domain]] row {i}: `domain` is empty");
            }
            if !domain_str.is_ascii() {
                anyhow::bail!(
                    "[[dkim.domain]] row {i} (domain={:?}): non-ASCII domain rejected (IDN labels not supported)",
                    row.domain
                );
            }
            let domain = Domain::new(domain_str);

            if row.selector.trim().is_empty() {
                anyhow::bail!(
                    "[[dkim.domain]] row {i} (domain={:?}): `selector` is empty",
                    domain.as_str()
                );
            }

            let algorithm = match row.algorithm.as_str() {
                "rsa-sha256" => DkimAlgorithm::RsaSha256,
                "ed25519-sha256" => DkimAlgorithm::Ed25519Sha256,
                other => anyhow::bail!(
                    "[[dkim.domain]] row {i} (domain={:?}): unknown algorithm {other:?} (want \"rsa-sha256\" or \"ed25519-sha256\")",
                    domain.as_str()
                ),
            };

            let canon = match row.canonicalization.as_ref() {
                None => (Canon::Relaxed, Canon::Relaxed),
                Some((h, b)) => (parse_canon(h, i, &domain)?, parse_canon(b, i, &domain)?),
            };

            let headers_to_sign = match row.headers.as_ref() {
                None => default_signed_headers(),
                Some(list) => list
                    .iter()
                    .map(|s| {
                        if let Some(rest) = s.strip_prefix('!') {
                            HeaderSpec::Oversign(rest.to_string())
                        } else {
                            HeaderSpec::Single(s.clone())
                        }
                    })
                    .collect(),
            };

            let pem = std::fs::read(&row.key_path).map_err(|e| {
                anyhow::anyhow!(
                    "[[dkim.domain]] row {i} (domain={:?}): failed to read key_path {}: {e}",
                    domain.as_str(),
                    row.key_path.display()
                )
            })?;
            MailAuthSigner::validate_pem(algorithm, &pem).map_err(|e| {
                anyhow::anyhow!(
                    "[[dkim.domain]] row {i} (domain={:?}, selector={:?}): key validation failed: {e}",
                    domain.as_str(),
                    row.selector
                )
            })?;

            if row.active_for_signing
                && let Some(prev) = active_seen.insert(domain.clone(), i)
            {
                anyhow::bail!(
                    "[[dkim.domain]] rows {prev} and {i} both have `active_for_signing = true` \
                     for domain {:?}: at most one active row per domain is allowed",
                    domain.as_str()
                );
            }

            out.push(DkimSignerConfig {
                domain,
                selector: row.selector.clone(),
                algorithm,
                private_key_pem: pem,
                canonicalization: canon,
                headers_to_sign,
                active_for_signing: row.active_for_signing,
                allow_body_length_tag: row.allow_body_length_tag,
            });
        }

        Ok(out)
    }
}

fn parse_canon(
    s: &str,
    row_index: usize,
    domain: &cosmix_maild_auth::Domain,
) -> Result<cosmix_maild_auth::Canon> {
    match s {
        "simple" => Ok(cosmix_maild_auth::Canon::Simple),
        "relaxed" => Ok(cosmix_maild_auth::Canon::Relaxed),
        other => anyhow::bail!(
            "[[dkim.domain]] row {row_index} (domain={:?}): unknown canonicalization {other:?} (want \"simple\" or \"relaxed\")",
            domain.as_str()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            hostname: "alpha.example.org".into(),
            ..Config::default()
        }
    }

    #[test]
    fn resolve_tls_identities_prefers_identity_list() {
        let mut cfg = base();
        cfg.tls_cert = Some("/legacy.crt".into());
        cfg.tls_key = Some("/legacy.key".into());
        cfg.tls.identity = vec![TlsIdentityConfig {
            server_name: "alpha.bus.example.org".into(),
            cert: "/wg.crt".into(),
            key: "/wg.key".into(),
            default: false,
            no_sni_fallback: false,
        }];
        let out = cfg.resolve_tls_identities();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].server_name, "alpha.bus.example.org");
        assert_eq!(out[0].cert, "/wg.crt");
    }

    #[test]
    fn resolve_tls_identities_collapses_legacy_pair() {
        let mut cfg = base();
        cfg.tls_cert = Some("/legacy.crt".into());
        cfg.tls_key = Some("/legacy.key".into());
        let out = cfg.resolve_tls_identities();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].server_name, "alpha.example.org");
        assert_eq!(out[0].cert, "/legacy.crt");
        assert_eq!(out[0].key, "/legacy.key");
        assert!(out[0].default, "legacy collapse must mark default");
        assert!(
            out[0].no_sni_fallback,
            "legacy collapse must serve clients without SNI"
        );
    }

    #[test]
    fn resolve_tls_identities_empty_when_nothing_set() {
        let cfg = base();
        assert!(cfg.resolve_tls_identities().is_empty());
    }

    #[test]
    fn resolve_tls_identities_half_legacy_is_empty() {
        // tls_cert without tls_key is operator error — surfacing
        // it later (at resolver-build time in step 2) is better
        // than half-collapsing here.
        let mut cfg = base();
        cfg.tls_cert = Some("/legacy.crt".into());
        let out = cfg.resolve_tls_identities();
        assert!(
            out.is_empty(),
            "half-set legacy pair must not collapse silently"
        );
    }

    #[test]
    fn strict_sni_default_is_false() {
        let cfg = base();
        assert!(!cfg.strict_sni());
    }

    #[test]
    fn config_parses_two_identity_block() {
        // `.conf.mix` strict-data form: map entries use `,` inside `{ }`.
        let src = r#"
            hostname: "alpha.example.org"
            tls: {
              identity: [
                { server_name: "alpha.example.org", cert: "/etc/cosmix/maild/tls/lan.crt", key: "/etc/cosmix/maild/tls/lan.key", default: true, no_sni_fallback: true },
                { server_name: "alpha.bus.example.org", cert: "/etc/cosmix/maild/tls/wg.crt", key: "/etc/cosmix/maild/tls/wg.key" }
              ]
            }
        "#;
        let cfg: Config = cosmix_config::from_conf_mix_str(src).expect("parse");
        assert_eq!(cfg.tls.identity.len(), 2);
        assert!(cfg.tls.identity[0].default);
        assert!(cfg.tls.identity[0].no_sni_fallback);
        assert!(!cfg.tls.identity[1].default);
    }

    #[test]
    fn config_numeric_fields_exact_safe_integers() {
        // Exercises the C1 exact-safe f64→int path on maild's numeric
        // config fields (Mix has no integer variant; these arrive as
        // Number(f64)). A whole-number value within the 2^53 lossless
        // range hydrates the Option<usize>/<u64> fields correctly.
        let src = r#"
            max_message_size: 52428800
            imap_max_literal_bytes: 104857600
            rule_stats_flush_interval_secs: 30
        "#;
        let cfg: Config = cosmix_config::from_conf_mix_str(src).expect("parse");
        assert_eq!(cfg.max_message_size, Some(52_428_800));
        assert_eq!(cfg.imap_max_literal_bytes, Some(104_857_600));
        assert_eq!(cfg.rule_stats_flush_interval_secs, Some(30));

        // A non-integral value for an integer field is rejected, not
        // truncated.
        let bad = "max_message_size: 3.5\n";
        assert!(cosmix_config::from_conf_mix_str::<Config>(bad).is_err());
    }

    #[test]
    fn bayesian_rebuild_operators_parses_and_defaults_open() {
        let defaulted: Config = cosmix_config::from_conf_mix_str("").expect("default config");
        assert!(defaulted.bayesian_rebuild_operators.is_empty());

        let configured: Config = cosmix_config::from_conf_mix_str(
            "bayesian_rebuild_operators: [ \"operator-one\", \"operator-two\" ]\n",
        )
        .expect("operator allowlist");
        assert_eq!(
            configured.bayesian_rebuild_operators,
            vec!["operator-one".to_string(), "operator-two".to_string()]
        );
    }

    #[test]
    fn config_listen_spec_untagged_both_arms() {
        // ListenSpec is `#[serde(untagged)]` — string or list. Both arms
        // must parse via the C1 deserialize_any path.
        let single = "smtp_inbound: \"192.0.2.4:25\"\n";
        let cfg: Config = cosmix_config::from_conf_mix_str(single).expect("single");
        assert_eq!(
            cfg.smtp_inbound.as_ref().unwrap().to_vec(),
            vec!["192.0.2.4:25".to_string()]
        );

        let multi = "smtp_inbound: [ \"192.0.2.4:25\", \"198.51.100.2:25\" ]\n";
        let cfg: Config = cosmix_config::from_conf_mix_str(multi).expect("multi");
        assert_eq!(
            cfg.smtp_inbound.as_ref().unwrap().to_vec(),
            vec!["192.0.2.4:25".to_string(), "198.51.100.2:25".to_string()]
        );
    }

    #[test]
    fn require_starttls_inbound_parses_and_defaults_empty() {
        // Omitted → empty (opportunistic everywhere, back-compat).
        let none = "smtp_inbound: \"192.0.2.4:25\"\n";
        let cfg: Config = cosmix_config::from_conf_mix_str(none).expect("no field");
        assert!(cfg.require_starttls_inbound.is_empty());

        // Present → the listed public bind requires STARTTLS while the
        // mesh bind (absent from the list) stays opportunistic.
        let src = "smtp_inbound: [ \"198.51.100.2:25\", \"203.0.113.5:25\" ]\n\
                   require_starttls_inbound: [ \"203.0.113.5:25\" ]\n";
        let cfg: Config = cosmix_config::from_conf_mix_str(src).expect("with field");
        assert_eq!(
            cfg.require_starttls_inbound,
            vec!["203.0.113.5:25".to_string()]
        );
    }

    #[test]
    fn validate_tls_rejects_explicit_empty_strings() {
        // `""` parses fine through TOML and serde; this is what the
        // step-1 missing-field guard *doesn't* catch on its own. The
        // load-time validation step must.
        let cases = [
            ("server_name", "", "/c", "/k"),
            ("cert", "host", "", "/k"),
            ("key", "host", "/c", ""),
            ("whitespace", "host", "   ", "/k"),
        ];
        for (label, sn, c, k) in cases {
            let mut cfg = base();
            cfg.tls.identity = vec![TlsIdentityConfig {
                server_name: sn.into(),
                cert: c.into(),
                key: k.into(),
                default: false,
                no_sni_fallback: false,
            }];
            assert!(
                cfg.validate_tls().is_err(),
                "{label}: empty/blank required field must fail validation"
            );
        }
    }

    #[test]
    fn validate_tls_passes_well_formed_identity() {
        let mut cfg = base();
        cfg.tls.identity = vec![TlsIdentityConfig {
            server_name: "host.example".into(),
            cert: "/c".into(),
            key: "/k".into(),
            default: true,
            no_sni_fallback: true,
        }];
        assert!(cfg.validate_tls().is_ok());
    }

    #[test]
    fn validate_tls_passes_empty_identity_list() {
        // No `[[tls.identity]]` rows is fine — legacy or no-TLS path.
        let cfg = base();
        assert!(cfg.validate_tls().is_ok());
    }

    // ---------- [[dkim.domain]] adapter tests ----------

    // mail-auth's published RSA-2048 test key, mirrored from
    // `cosmix-maild-auth::signer::tests::RSA_TEST_PEM`. Embedding the
    // bytes inline keeps these tests `#[cfg(test)]`-only and avoids a
    // dev-dep on the auth crate's private fixtures.
    const RSA_TEST_PEM: &[u8] = b"\
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAv9XYXG3uK95115mB4nJ37nGeNe2CrARm1agrbcnSk5oIaEfM
ZLUR/X8gPzoiNHZcfMZEVR6bAytxUhc5EvZIZrjSuEEeny+fFd/cTvcm3cOUUbIa
UmSACj0dL2/KwW0LyUaza9z9zor7I5XdIl1M53qVd5GI62XBB76FH+Q0bWPZNkT4
NclzTLspD/MTpNCCPhySM4Kdg5CuDczTH4aNzyS0TqgXdtw6A4Sdsp97VXT9fkPW
9rso3lrkpsl/9EQ1mR/DWK6PBmRfIuSFuqnLKY6v/z2hXHxF7IoojfZLa2kZr9Ae
d4l9WheQOTA19k5r2BmlRw/W9CrgCBo0Sdj+KQIDAQABAoIBAFPChEi/OvnulReB
ECQWhOUYuNKlFKQU++2YEvZJ4+bMn5UgnE7wfJ1pj2Pr9xlfALz+OMHNrjMxGbaV
KzdrT2uCkYcf78XjnhuH9gKIiXDUv4L4N+P3u6w8yOx4bFgOS9IjS53yDOPM7SC5
g6dIg5aigHaHlffqIuFFv4yQMI/+Ai+zBKxS7wRhxK/7nnAuo28fe5MEdp57ho9/
AGlDNsdg9zCgjwhokwFE3+AaD+bkUFm4gQ1XjkUFrlmnQn8vDQ0i9toEWhCj+UPY
iOKL63MJnr90MXTXWLHoFj99wBp//mYygbF9Lj8fa28/oa8LWp3Jhb7QeMgH46iv
3aLHbTECgYEA5M2dAw+nyMw9vYlkMejhwObKYP8Mr/6zcGMLCalYvRJM5iUAM0JI
H6sM6pV9/nv167cbKocj3xYPdtE7FPOn4132MLM8Ne1f8nPE64Qrcbj5WBXvLnU8
hpWbwe2Z8h7UUMKx6q4F1/TXYkc3ScxYwfjM4mP/pLsAOgVzRSEEgrUCgYEA1qNQ
xaQHNWZ1O8WuTnqWd5JSsic6iURAmUcLeFDZY2PWhVoaQ8L/xMQhDYs1FIbLWArW
4Qq3Ibu8AbSejAKuaJz7Uf26PX+PYVUwAOO0qamCJ8d/qd6So7qWMDyAY2yXI39Y
1nMqRjr7bkEsggAZao7BKqA7ZtmogjOusBT38iUCgYEA06agJ8TDoKvOMRZ26PRU
YO0dKLzGL8eclcoI29cbj0rud7aiiMg3j5PbTuUat95TjsjDCIQaWrM9etvxm2AJ
Xfn9Uu96MyhyKQWOk46f4YMKpMElkARDCPw8KRhx39dE77AqhLyWCz8iPndCXbH6
KPTOEl4OjYOuof2Is9nnIkECgYBh948RdsnXhNlzm8nwhiGRmBbou+EK8D0v+O5y
Tyy6IcKzgSnFzgZh8EdJ4EUtBk1f9SqY8wQdgIvSl3daXorusuA/TzkngsaV3YUY
ktZOLlF7CKLrjOyPkMWmZKcROmpNyH1q/IvKHHfQnizLdXIkYd4nL5WNX0F7lE1i
j1+QhQKBgB2lviBK7rJFwlFYdQUP1NAN2dKxMZk8uJS8JglHrM0+8nRI83HbTdEQ
vB0ManEKBkbS4T5n+gRtdEqKSDmWDTXDlrBfcdCHNQLwYtBpOotCqQn/AmfjcPBl
byAbwh4+HiZ5JISoRZpiZqy67aJNVoXmdtb/E9mi7ozzytpxMNql
-----END RSA PRIVATE KEY-----
";

    fn write_rsa_key(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, RSA_TEST_PEM).expect("write test key");
        p
    }

    fn dkim_row(domain: &str, selector: &str, key_path: PathBuf) -> DkimDomainConfig {
        DkimDomainConfig {
            domain: domain.into(),
            selector: selector.into(),
            algorithm: "rsa-sha256".into(),
            key_path,
            canonicalization: None,
            headers: None,
            active_for_signing: true,
            allow_body_length_tag: false,
        }
    }

    #[test]
    fn dkim_adapter_empty_returns_empty() {
        let cfg = base();
        let out = cfg.resolve_dkim_signer_configs().expect("ok");
        assert!(out.is_empty());
    }

    #[test]
    fn dkim_adapter_happy_path_single_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("example.org", "s1", kp)];
        let out = cfg.resolve_dkim_signer_configs().expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].domain.as_str(), "example.org");
        assert_eq!(out[0].selector, "s1");
        assert!(out[0].active_for_signing);
    }

    #[test]
    fn dkim_adapter_lowercases_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("ExAmPle.ORG", "s1", kp)];
        let out = cfg.resolve_dkim_signer_configs().expect("ok");
        assert_eq!(out[0].domain.as_str(), "example.org");
    }

    #[test]
    fn dkim_adapter_rejects_empty_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("   ", "s1", kp)];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("`domain` is empty"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_non_ascii_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("exämple.org", "s1", kp)];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("non-ASCII"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_empty_selector() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("example.org", "  ", kp)];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("`selector` is empty"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_unknown_algorithm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        let mut row = dkim_row("example.org", "s1", kp);
        row.algorithm = "md5-foo".into();
        cfg.dkim.domains = vec![row];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("unknown algorithm"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_unknown_canonicalization() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        let mut row = dkim_row("example.org", "s1", kp);
        row.canonicalization = Some(("simple".into(), "weird".into()));
        cfg.dkim.domains = vec![row];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("unknown canonicalization"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_parses_oversign_prefix() {
        use cosmix_maild_auth::HeaderSpec;
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        let mut row = dkim_row("example.org", "s1", kp);
        row.headers = Some(vec!["!From".into(), "To".into(), "Subject".into()]);
        cfg.dkim.domains = vec![row];
        let out = cfg.resolve_dkim_signer_configs().expect("ok");
        let h = &out[0].headers_to_sign;
        assert!(matches!(&h[0], HeaderSpec::Oversign(n) if n == "From"));
        assert!(matches!(&h[1], HeaderSpec::Single(n) if n == "To"));
        assert!(matches!(&h[2], HeaderSpec::Single(n) if n == "Subject"));
    }

    #[test]
    fn dkim_adapter_rejects_dual_active_per_domain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp1 = write_rsa_key(dir.path(), "s1.pem");
        let kp2 = write_rsa_key(dir.path(), "s2.pem");
        let mut cfg = base();
        cfg.dkim.domains = vec![
            dkim_row("example.org", "s1", kp1),
            dkim_row("example.org", "s2", kp2),
        ];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(
            err.contains("both have `active_for_signing = true`"),
            "got: {err}"
        );
    }

    #[test]
    fn dkim_adapter_allows_one_active_one_standby() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp1 = write_rsa_key(dir.path(), "s1.pem");
        let kp2 = write_rsa_key(dir.path(), "s2.pem");
        let mut cfg = base();
        let mut standby = dkim_row("example.org", "s2", kp2);
        standby.active_for_signing = false;
        cfg.dkim.domains = vec![dkim_row("example.org", "s1", kp1), standby];
        let out = cfg.resolve_dkim_signer_configs().expect("ok");
        assert_eq!(out.len(), 2);
        assert!(out[0].active_for_signing);
        assert!(!out[1].active_for_signing);
    }

    #[test]
    fn dkim_adapter_reports_missing_key_path() {
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row(
            "example.org",
            "s1",
            PathBuf::from("/nonexistent/dkim/key.pem"),
        )];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("failed to read key_path"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_garbage_pem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = dir.path().join("bad.pem");
        std::fs::write(&kp, b"not a pem file at all").expect("write");
        let mut cfg = base();
        cfg.dkim.domains = vec![dkim_row("example.org", "s1", kp)];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("key validation failed"), "got: {err}");
    }

    #[test]
    fn dkim_adapter_rejects_rsa_pem_under_ed25519_algorithm() {
        // Same RSA bytes, but operator declared ed25519-sha256 —
        // `validate_pem` must catch the mismatch before sign time.
        let dir = tempfile::tempdir().expect("tempdir");
        let kp = write_rsa_key(dir.path(), "s1.pem");
        let mut cfg = base();
        let mut row = dkim_row("example.org", "s1", kp);
        row.algorithm = "ed25519-sha256".into();
        cfg.dkim.domains = vec![row];
        let err = cfg.resolve_dkim_signer_configs().unwrap_err().to_string();
        assert!(err.contains("key validation failed"), "got: {err}");
    }
}
