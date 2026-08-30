//! Listener specification types — the bootstrap description of one
//! logical listener (its bind addresses, TLS mode, and guard policy).
//!
//! Config *deserialization* (the `ListenSpec` string-or-array shape,
//! the node.conf.mix `[[*.listener]]` rows) stays in each daemon /
//! `cosmix-lib-config`; this module takes already-resolved
//! [`SocketAddr`]s so the runtime layer is config-format-agnostic.

use std::net::SocketAddr;

use crate::listen::guard::GuardPolicy;

/// How the accept driver treats TLS for a listener.
///
/// The TLS-terminating modes only exist when the crate's `tls`
/// feature is on; a default-features (plain-TCP) consumer sees only
/// [`TlsMode::Plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    /// Plain TCP. The handler receives [`AcceptedStream::Tcp`].
    ///
    /// [`AcceptedStream::Tcp`]: crate::listen::handler::AcceptedStream::Tcp
    #[default]
    Plain,
    /// The crate terminates TLS and hands the handler an
    /// [`AcceptedStream::Tls`] (webd HTTPS, maild SMTPS/IMAPS).
    ///
    /// [`AcceptedStream::Tls`]: crate::listen::handler::AcceptedStream::Tls
    #[cfg(feature = "tls")]
    Terminate,
    /// The crate hands the handler the raw TCP stream plus the
    /// listener's TLS material in [`ConnCtx::tls`] so the handler can
    /// drive STARTTLS itself (maild SMTP :25 / IMAP :143).
    ///
    /// [`ConnCtx::tls`]: crate::listen::handler::ConnCtx::tls
    #[cfg(feature = "tls")]
    StartTlsPassthrough,
}

/// What to do when some — but not all — of a listener's bind
/// addresses fail to bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindPolicy {
    /// Any bind failure aborts the whole listener (no silently
    /// half-up dual-bind). The default.
    #[default]
    FailAll,
    /// Bind what we can; warn on the rest. For the maild WG+LAN
    /// dual-bind where one interface may legitimately be down at
    /// boot. The listener still fails if *zero* addresses bind.
    BestEffort,
}

/// Bootstrap description of one logical listener.
///
/// `enabled` here is the *config* default consulted by
/// [`ListenerSet::start_all`]; the live on/off state after start is
/// owned by [`ListenerSetControl`].
///
/// [`ListenerSet::start_all`]: crate::listen::ListenerSet::start_all
/// [`ListenerSetControl`]: crate::listen::ListenerSetControl
#[derive(Debug, Clone)]
pub struct ListenerSpec {
    /// Stable id — the key for control verbs, logs, and the
    /// `listener_id` connection tag. Unique within a [`ListenerSet`].
    ///
    /// [`ListenerSet`]: crate::listen::ListenerSet
    pub id: String,
    /// One or more resolved bind addresses. More than one = a
    /// multi-home listener (e.g. WG + LAN) sharing one handler and
    /// one guard budget.
    pub binds: Vec<SocketAddr>,
    /// Is this a public / untrusted-facing bind? Drives a startup
    /// log line, the `external` flag on [`ConnCtx`], and (for the
    /// daemon) guard defaults. It does NOT itself change bind
    /// behaviour — the address does.
    ///
    /// [`ConnCtx`]: crate::listen::handler::ConnCtx
    pub external: bool,
    /// Bootstrap enable. A disabled listener is held in the set but
    /// not bound by [`start_all`](crate::listen::ListenerSet::start_all).
    pub enabled: bool,
    /// TLS termination mode.
    pub tls_mode: TlsMode,
    /// Per-listener connection guard policy (rate, caps, ACL,
    /// strict-SNI). All-default-off.
    pub guard: GuardPolicy,
    /// What to do on a partial multi-bind failure.
    pub bind_policy: BindPolicy,
}

impl ListenerSpec {
    /// A plain-TCP listener on `binds`, enabled, no guards. Chain the
    /// `with_*` setters to specialise.
    pub fn new(id: impl Into<String>, binds: Vec<SocketAddr>) -> Self {
        Self {
            id: id.into(),
            binds,
            external: false,
            enabled: true,
            tls_mode: TlsMode::default(),
            guard: GuardPolicy::default(),
            bind_policy: BindPolicy::default(),
        }
    }

    /// Set the TLS termination mode.
    pub fn with_tls_mode(mut self, mode: TlsMode) -> Self {
        self.tls_mode = mode;
        self
    }

    /// Mark this listener external (public-facing).
    pub fn with_external(mut self, external: bool) -> Self {
        self.external = external;
        self
    }

    /// Set the bootstrap enable flag.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Attach a guard policy.
    pub fn with_guard(mut self, guard: GuardPolicy) -> Self {
        self.guard = guard;
        self
    }

    /// Set the partial-bind policy.
    pub fn with_bind_policy(mut self, policy: BindPolicy) -> Self {
        self.bind_policy = policy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_are_plain_enabled_unguarded() {
        let s = ListenerSpec::new("wg", vec!["127.0.0.1:8443".parse().unwrap()]);
        assert_eq!(s.id, "wg");
        assert_eq!(s.tls_mode, TlsMode::Plain);
        assert!(s.enabled);
        assert!(!s.external);
        assert_eq!(s.bind_policy, BindPolicy::FailAll);
        assert!(s.guard.per_ip_rate.is_none());
    }

    #[test]
    fn builders_chain() {
        let s = ListenerSpec::new("pub", vec!["127.0.0.1:0".parse().unwrap()])
            .with_external(true)
            .with_enabled(false)
            .with_bind_policy(BindPolicy::BestEffort);
        assert!(s.external);
        assert!(!s.enabled);
        assert_eq!(s.bind_policy, BindPolicy::BestEffort);
    }
}
