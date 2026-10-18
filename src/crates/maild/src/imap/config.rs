//! IMAP server configuration (Phase 1).
//!
//! Mirrors the Phase-1 TOML surface in `_doc/maild/imap.md`. Defaults
//! match the design-doc baseline; production configs override
//! selectively (e.g. tighten `pre_auth_timeout_secs` to 30 s once the
//! real-client matrix is green).

use std::sync::Arc;
use std::time::Duration;

use crate::tls::{ServerConfigCache, TlsSlot};

/// Phase-1 IMAP server configuration.
///
/// Construct via `ImapConfig::default()` and override fields; or use
/// [`ImapConfig::from_config`] to lift values from the top-level
/// [`crate::config::Config`].
#[derive(Debug, Clone)]
pub struct ImapConfig {
    /// IMAPS listen addresses (port 993). Empty = disabled.
    pub listen_imaps: Vec<String>,
    /// Server hostname advertised in greeting / capability banner.
    pub hostname: String,
    /// Live-swappable TLS resolver slot, shared with the SMTP stack.
    /// The IMAPS accept loop calls
    /// [`crate::tls::current_tls_pair`] once per accepted connection
    /// and threads the captured resolver into
    /// [`crate::imap::session::handle_connection`] for greeting
    /// hostname recovery. Empty slot (`load_full() == None`) means
    /// TLS is disabled — the listener refuses to start.
    pub tls_slot: TlsSlot,
    /// Per-resolver `Arc<ServerConfig>` cache shared with the SMTP
    /// stack. The Phase 5a-commit-2 reload verb's `clear()` after
    /// `slot.store(...)` invalidates both surfaces at once.
    pub tls_config_cache: Arc<ServerConfigCache>,
    /// Maximum literal size accepted on APPEND. 64 MiB default.
    /// Phase 1 has no literal-accepting verbs; the field is wired
    /// through so Phase 4 inherits the value without churning the
    /// config surface.
    pub max_literal_bytes: u64,
    /// Untagged `* OK Still here` interval for IDLE keepalive.
    /// Phase 5 wiring; Phase 1 stores the value but does not
    /// advertise IDLE behaviour.
    pub idle_status_interval: Duration,
    /// Hard timeout for pre-auth connections. Defaults to 180 s for
    /// the implementation phase; tighten to 30 s once the matrix
    /// passes.
    pub pre_auth_timeout: Duration,
    /// Per-connection auth failure cap. The connection is closed
    /// with `* BYE` after `max_auth_failures` consecutive
    /// `NO [AUTHENTICATIONFAILED]` responses.
    pub max_auth_failures: u32,
    /// Pre-auth bad-command cap. `BAD` responses past this trigger
    /// `* BYE [CLIENTBUG]` and disconnect.
    pub max_bad_commands_pre_auth: u32,
    /// Post-auth bad-command cap. Same semantics, looser bound.
    pub max_bad_commands_post_auth: u32,
    /// Per-account concurrent connection cap. New connections beyond
    /// the cap receive `* BYE [INUSE] too many connections` and are
    /// closed immediately after a successful authenticate.
    pub max_concurrent_per_account: u32,
    /// Operator override of the advertised capability list. When
    /// `None` (default), [`crate::imap::capabilities::INITIAL`] is
    /// used.
    pub advertise_capabilities: Option<Vec<String>>,
}

impl Default for ImapConfig {
    fn default() -> Self {
        Self {
            listen_imaps: Vec::new(),
            hostname: "localhost".into(),
            tls_slot: crate::tls::new_tls_slot(None),
            tls_config_cache: Arc::new(ServerConfigCache::new()),
            max_literal_bytes: 64 * 1024 * 1024,
            idle_status_interval: Duration::from_secs(120),
            pre_auth_timeout: Duration::from_secs(180),
            max_auth_failures: 3,
            max_bad_commands_pre_auth: 3,
            max_bad_commands_post_auth: 20,
            max_concurrent_per_account: 20,
            advertise_capabilities: None,
        }
    }
}

impl ImapConfig {
    /// Lift IMAP-specific fields from the top-level [`crate::config::Config`].
    /// Unset values fall back to [`ImapConfig::default`].
    ///
    /// Zero-value overrides for the operational caps
    /// (`max_auth_failures`, `max_bad_commands_*`,
    /// `max_concurrent_per_account`, `pre_auth_timeout_secs`,
    /// `idle_status_interval_secs`) are silently dropped: a zero
    /// there immediately disconnects every accepted connection
    /// (auth-failure cap=0 → BYE after the first credential check;
    /// concurrent cap=0 → every authenticated user gets INUSE) and
    /// is almost certainly an operator typo rather than an intended
    /// "disable the cap" knob. The default value is preserved
    /// instead. If a real "disable" semantics is needed later, add
    /// an explicit sentinel rather than overloading `0`.
    pub fn from_config(
        cfg: &crate::config::Config,
        tls_slot: TlsSlot,
        tls_config_cache: Arc<ServerConfigCache>,
    ) -> Self {
        let mut out = ImapConfig {
            hostname: cfg.hostname.clone(),
            tls_slot,
            tls_config_cache,
            ..ImapConfig::default()
        };
        if let Some(spec) = &cfg.imap_imaps {
            out.listen_imaps = spec.to_vec();
        }
        if let Some(v) = cfg.imap_max_literal_bytes {
            out.max_literal_bytes = v;
        }
        if let Some(v) = cfg.imap_idle_status_interval_secs
            && v > 0
        {
            out.idle_status_interval = Duration::from_secs(v);
        }
        if let Some(v) = cfg.imap_pre_auth_timeout_secs
            && v > 0
        {
            out.pre_auth_timeout = Duration::from_secs(v);
        }
        if let Some(v) = cfg.imap_max_auth_failures
            && v > 0
        {
            out.max_auth_failures = v;
        }
        if let Some(v) = cfg.imap_max_bad_commands_pre_auth
            && v > 0
        {
            out.max_bad_commands_pre_auth = v;
        }
        if let Some(v) = cfg.imap_max_bad_commands_post_auth
            && v > 0
        {
            out.max_bad_commands_post_auth = v;
        }
        if let Some(v) = cfg.imap_max_concurrent_per_account
            && v > 0
        {
            out.max_concurrent_per_account = v;
        }
        if let Some(caps) = cfg.imap_advertise_capabilities.clone() {
            out.advertise_capabilities = Some(caps);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn from_config_drops_zero_operational_caps() {
        let cfg = Config {
            imap_max_auth_failures: Some(0),
            imap_max_bad_commands_pre_auth: Some(0),
            imap_max_bad_commands_post_auth: Some(0),
            imap_max_concurrent_per_account: Some(0),
            imap_pre_auth_timeout_secs: Some(0),
            imap_idle_status_interval_secs: Some(0),
            ..Config::default()
        };
        let out = ImapConfig::from_config(
            &cfg,
            crate::tls::new_tls_slot(None),
            Arc::new(ServerConfigCache::new()),
        );
        let dflt = ImapConfig::default();
        assert_eq!(out.max_auth_failures, dflt.max_auth_failures);
        assert_eq!(
            out.max_bad_commands_pre_auth,
            dflt.max_bad_commands_pre_auth
        );
        assert_eq!(
            out.max_bad_commands_post_auth,
            dflt.max_bad_commands_post_auth
        );
        assert_eq!(
            out.max_concurrent_per_account,
            dflt.max_concurrent_per_account
        );
        assert_eq!(out.pre_auth_timeout, dflt.pre_auth_timeout);
        assert_eq!(out.idle_status_interval, dflt.idle_status_interval);
    }

    #[test]
    fn from_config_honors_nonzero_overrides() {
        let cfg = Config {
            imap_max_auth_failures: Some(5),
            imap_max_concurrent_per_account: Some(7),
            ..Config::default()
        };
        let out = ImapConfig::from_config(
            &cfg,
            crate::tls::new_tls_slot(None),
            Arc::new(ServerConfigCache::new()),
        );
        assert_eq!(out.max_auth_failures, 5);
        assert_eq!(out.max_concurrent_per_account, 7);
    }
}
