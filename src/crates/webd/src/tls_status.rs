//! TLS status snapshot — the value carried over the `tokio::sync::watch`
//! channel that backs `webd.tls.status`.
//!
//! Two owners write into the channel:
//!
//! 1. `main` seeds it with [`TlsStatusSnapshot::initial`] before the
//!    ACME branch, so manual-PEM-only deployments — which never
//!    construct an [`crate::acme_provisioner::AcmeProvisioner`] — still
//!    expose `webd.tls.status` with the resolved manual identities.
//! 2. The provisioner overwrites it via `tx.send_replace(...)` once
//!    after [`crate::acme_provisioner::AcmeProvisioner::startup_pass`]
//!    completes, then again after every state-mutating branch of its
//!    `run_forever` loop (each successful issuance, each backoff
//!    advance). Publishing only at the top of each ~6h tick would
//!    leave `last_error` / `next_attempt` stale until the next sweep,
//!    which defeats the verb's purpose — operators want fresh failure
//!    detail, not yesterday's snapshot.
//!
//! The Bus layer holds a `watch::Receiver<TlsStatusSnapshot>` (via
//! [`crate::NodeState::tls_status_rx`]) and reads `rx.borrow()` on
//! each dispatch — non-blocking, no lock contention with the renewal
//! loop, no ownership conflict with `AcmeProvisioner::run_forever`
//! (which consumes `self`).

use std::collections::BTreeMap;

use cosmix_config::acme_policy::AcmeVhostPlan;
use cosmix_config::node::{TlsIdentityConfig, WebdAcmeProvider};
use serde::Serialize;

/// Whole snapshot returned by `webd.tls.status`. `acme` is `None`
/// when the node has no ACME plans configured (manual-PEM-only
/// deployments and HTTP-only deployments alike).
///
/// `Clone` is required by `watch::channel` — clones are infrequent
/// (per-dispatch read via `rx.borrow()` is *not* a clone) so the cost
/// is acceptable.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TlsStatusSnapshot {
    pub manual_identities: Vec<ManualIdentitySnapshot>,
    pub acme: Option<AcmeStatusSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManualIdentitySnapshot {
    pub server_name: String,
    pub default: bool,
    pub no_sni_fallback: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AcmeStatusSnapshot {
    pub plans: Vec<AcmePlanSummary>,
    pub vhost_state: BTreeMap<String, AcmeVhostStateSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AcmePlanSummary {
    pub fqdn: String,
    pub aliases: Vec<String>,
    pub provider: WebdAcmeProvider,
}

/// Per-vhost runtime issuance state. `next_attempt_after` is rendered
/// as RFC-3339 UTC at publish time so the wire form stays self-
/// contained (no `time` crate dependency on consumers) and the
/// watch-channel value implements `Clone` without `OffsetDateTime`
/// re-serialisation surprises.
///
/// `issued` is derived from
/// [`crate::acme_provisioner::AcmeProvisioner::acme_identities`] —
/// `true` iff a live ACME chain is currently spliced into the
/// resolver for this FQDN. Operators reading the verb's output use
/// this to distinguish "configured but never issued" from "issued
/// and renewing".
#[derive(Clone, Debug, Default, Serialize)]
pub struct AcmeVhostStateSnapshot {
    pub issued: bool,
    pub last_error: Option<String>,
    pub last_error_count: u32,
    pub next_attempt_after_rfc3339: Option<String>,
}

impl TlsStatusSnapshot {
    /// Build the seed snapshot from the resolved manual-PEM identities.
    /// `acme` is `None`: the provisioner overwrites it after
    /// `startup_pass` if ACME plans exist; otherwise it stays `None`
    /// for the daemon's lifetime.
    pub fn initial(manual: &[TlsIdentityConfig]) -> Self {
        Self {
            manual_identities: manual.iter().map(ManualIdentitySnapshot::from).collect(),
            acme: None,
        }
    }
}

impl From<&TlsIdentityConfig> for ManualIdentitySnapshot {
    fn from(id: &TlsIdentityConfig) -> Self {
        Self {
            server_name: id.server_name.clone(),
            default: id.default,
            no_sni_fallback: id.no_sni_fallback,
        }
    }
}

impl From<&AcmeVhostPlan> for AcmePlanSummary {
    fn from(p: &AcmeVhostPlan) -> Self {
        Self {
            fqdn: p.fqdn.clone(),
            aliases: p.aliases.clone(),
            provider: p.provider,
        }
    }
}
