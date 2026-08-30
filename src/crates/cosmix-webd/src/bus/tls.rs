//! `webd.tls.{status,reload}` — TLS posture inspection + live
//! manual-PEM cert reload.
//!
//! ## `webd.tls.status`
//!
//! Reads from the [`tokio::sync::watch`] mirror owned by the
//! [`crate::acme_provisioner::AcmeProvisioner`] (when ACME exists) or
//! seeded by `main` from the resolved manual-PEM identities
//! otherwise. `rx.borrow()` is non-blocking and contends with no
//! mutator — see [`crate::tls_status`] for the snapshot shape and
//! publish-point discipline.
//!
//! ## `webd.tls.reload` (B2)
//!
//! Picks up renewed **manual-PEM** certs WITHOUT a `systemctl restart`:
//! re-reads + re-`validate_le_chain`s the configured cert files, rebuilds
//! one [`SniCertResolver`] per listener (build-all-before-swap), atomic-
//! swaps each [`ListenerTls`], and republishes the `tls.status` watch
//! snapshot. Failure posture is **fail-loud-and-keep**: if any cert fails
//! to read/validate, or a resolver fails to build (e.g. key↔cert
//! mismatch), the verb returns `rc=10` and every listener keeps its prior
//! resolver — no half-valid swap, in-flight handshakes undisturbed.
//!
//! The verb is **manual-PEM only**. On a node with an active ACME
//! provisioner the per-listener resolvers are owned by that provisioner
//! (it merges the manual floor with the ACME identities on every
//! republish, re-reading the manual cert files in the process), so an
//! independent manual swap here would race it and drop the ACME certs
//! until the next renewal tick. Those nodes refresh manual certs via
//! `webd.acme.renew` instead; `webd.tls.reload` returns `rc=10` with a
//! pointer. `NodeState::tls_reload` is therefore `Some` only on a
//! manual-PEM-capable node (no provisioner + at least one TLS listener).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use cosmix_client::IncomingCommand;
use cosmix_config::node::{ResolvedWebdListener, TlsIdentityConfig};
use cosmix_daemon::listen::ListenerTls;
use cosmix_daemon::tls::le_validator::validate_le_chain;
use cosmix_daemon::tls::sni::SniCertResolver;
use cosmix_props::Runtime;
use rustls::pki_types::UnixTime;
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use tokio::sync::{Mutex, watch};

use crate::NodeState;
use crate::listeners_namespace;
use crate::tls_status::TlsStatusSnapshot;

/// Capability gating `webd.tls.reload`. The reload mutates the
/// per-listener TLS resolvers, so it is gated to the SAME operator tier
/// as the `webd.listener.{enable,disable}` kill switch — a caller in the
/// `[webd.listeners] operators` L0 allowlist (resolved via
/// `listeners_namespace::auth_policy`, which `listener_verbs::require_cap`
/// uses). Reusing the existing operator cap means no new policy grant to
/// register and keeps "who may mutate listener/TLS state" a single tier.
/// An anonymous caller (e.g. the bare `cosmix-webd tls reload` CLI on a
/// node where `""` is not an operator) is denied — operate via an
/// identity in the operators allowlist, as with the kill switch.
const RELOAD_CAP: &str = "props.write:webd.listeners";

const RC_OK: u8 = 0;
const RC_ERROR: u8 = 10;

pub fn status_body(node: &Arc<NodeState>) -> String {
    // `serde_json::to_string(...)` rather than `json!({...})` because
    // `TlsStatusSnapshot` already derives `Serialize`; the on-the-wire
    // shape is the snapshot serialised directly.
    let snap = node.tls_status_rx.borrow();
    serde_json::to_string(&*snap).unwrap_or_else(|_| {
        // Serialise failures shouldn't reach the wire as garbled
        // bytes — fall back to an empty object so the response stays
        // well-formed JSON. `TlsStatusSnapshot` carries only owned
        // strings + small enums, so this branch is unreachable in
        // practice; the fallback exists so `webd.tls.status` cannot
        // panic the Bus dispatcher.
        "{}".to_string()
    })
}

/// Shared state for the `webd.tls.reload` verb. Clone-cheap — every
/// field is `Arc`-shaped or a small owned map captured once at startup.
/// Constructed by `main` only on a manual-PEM-capable node (see the
/// module docs); absent (`NodeState::tls_reload == None`) otherwise.
pub struct TlsReloadState {
    /// Per-listener live resolver handles, shared with the accept loop.
    /// A successful reload `swap`s a freshly-built resolver into each.
    tls_listeners: HashMap<String, ListenerTls>,
    /// The manual-PEM identity set resolved at startup (server_name +
    /// cert/key **paths**). The paths are stable across a reload — only
    /// the file *contents* change on a renewal — so re-reading them
    /// picks up the new cert. Frozen at startup: changing the identity
    /// SET still needs a daemon restart (it comes from `[[webd.vhost]]`).
    manual_identities: Vec<TlsIdentityConfig>,
    /// Resolved listener set — the host→listener partition key for
    /// rebuilding each listener's bucket via
    /// [`crate::partition_identities_by_listener`].
    resolved_listeners: Vec<ResolvedWebdListener>,
    /// `webd.listeners` namespace runtime. The reload snapshots it
    /// **at reload time** to read each listener's *live* `strict_sni`
    /// (L1-authoritative) — so an operator who flips `strict_sni` via
    /// `props.set webd.listeners` then reloads gets the new posture,
    /// rather than the reload reverting to a startup-frozen value.
    listeners_runtime: Arc<Runtime>,
    /// `webd.tls.status` watch sender. Republished after a successful
    /// swap so the status verb reflects the reload. `Option` for
    /// defensiveness — on a manual-PEM node `main` always hands the
    /// sender here (the provisioner, which would otherwise own it, does
    /// not exist), so it is `Some` in practice.
    tls_status_tx: Option<watch::Sender<TlsStatusSnapshot>>,
    /// Serializes concurrent reloads so the read-validate-build-swap
    /// sequence is never interleaved (last-writer-wins on the swap,
    /// correct for a refresh primitive).
    reload_lock: Arc<Mutex<()>>,
}

impl TlsReloadState {
    pub fn new(
        tls_listeners: HashMap<String, ListenerTls>,
        manual_identities: Vec<TlsIdentityConfig>,
        resolved_listeners: Vec<ResolvedWebdListener>,
        listeners_runtime: Arc<Runtime>,
        tls_status_tx: Option<watch::Sender<TlsStatusSnapshot>>,
    ) -> Self {
        Self {
            tls_listeners,
            manual_identities,
            resolved_listeners,
            listeners_runtime,
            tls_status_tx,
            reload_lock: Arc::new(Mutex::new(())),
        }
    }
}

/// Dispatch a `webd.tls.*` mutation verb. Returns `Some((rc, body))`
/// for `tls.reload`; `None` for anything else (notably the read-only
/// `tls.status`, which stays on the synchronous `dispatch` path).
pub async fn dispatch(
    suffix: &str,
    cmd: &IncomingCommand,
    node: &Arc<NodeState>,
) -> Option<(u8, String)> {
    match suffix {
        "tls.reload" => Some(
            // Mutating verb — gate it to the listeners operator tier (the
            // same gate as the `webd.listener.*` kill switch;
            // `feedback_bus_wire_trust_boundary`: the WG trust domain does
            // not make callers non-adversarial). Cap-check first so an
            // unauthorised caller can't even probe the node's TLS posture
            // via the availability message.
            match crate::bus::listener_verbs::require_cap(node, cmd, RELOAD_CAP) {
                Err(rsp) => rsp,
                Ok(()) => match &node.tls_reload {
                    Some(state) => to_response(handle_reload(state).await),
                    None => (
                        RC_ERROR,
                        err_body(
                            "webd.tls.reload unavailable on this node: it has no \
                             manual-PEM TLS listener to reload. An ACME-managed node \
                             refreshes manual certs via `webd.acme.renew` (its \
                             provisioner owns the per-listener resolvers); an \
                             HTTP-only node has no TLS surface.",
                        ),
                    ),
                },
            },
        ),
        _ => None,
    }
}

async fn handle_reload(state: &TlsReloadState) -> Result<JsonValue> {
    let _guard = state.reload_lock.lock().await;

    // One clock read for the whole validation pass — avoid a micro-skew
    // splitting "valid" / "expired" across two certs.
    let now = unix_now()?;

    // 1. Group identities by cert path so each cert is re-validated ONCE
    //    against EVERY name it must cover — startup parity, where
    //    `validate_le_chain` runs per-vhost over host + aliases (which
    //    flatten into N identities sharing one cert path here).
    let mut names_by_cert: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in &state.manual_identities {
        names_by_cert
            .entry(id.cert.clone())
            .or_default()
            .push(id.server_name.clone());
    }

    // 2. Re-read + re-validate each cert. Collect ALL failures so the
    //    operator sees every bad cert at once; swap nothing if any fail.
    let mut failures: Vec<String> = Vec::new();
    for (cert_path, names) in &names_by_cert {
        let pem = match std::fs::read(cert_path) {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("{cert_path}: read failed: {e}"));
                continue;
            }
        };
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        if let Err(e) = validate_le_chain(&pem, &refs, now) {
            failures.push(format!(
                "{cert_path} (covers {names:?}): validate_le_chain: {e}"
            ));
        }
    }
    if !failures.is_empty() {
        return Err(anyhow!(
            "manual-PEM reload refused — {} cert(s) failed validation, prior resolver \
             kept on every listener: {}",
            failures.len(),
            failures.join("; ")
        ));
    }

    // 3. Read each listener's LIVE strict_sni from the L1-authoritative
    //    `webd.listeners` namespace, so an operator who flipped
    //    strict_sni at runtime gets the new posture instead of a
    //    startup-frozen value reverting it.
    let strict_by_id: HashMap<String, bool> =
        listeners_namespace::snapshot_rows(&state.listeners_runtime)
            .await
            .map_err(|e| anyhow!("snapshotting webd.listeners for live strict_sni: {e}"))?
            .into_iter()
            .map(|r| (r.id, r.strict_sni))
            .collect();

    // 4. Rebuild every listener's resolver from its manual-PEM bucket.
    //    `SniCertResolver::from_config` re-reads the PEM + key files and
    //    validates the key↔cert match (a failure here also keeps the old
    //    resolver — we build ALL before swapping ANY).
    let buckets = crate::partition_identities_by_listener(
        &state.manual_identities,
        &state.resolved_listeners,
    );
    let mut built: Vec<(&ListenerTls, Arc<SniCertResolver>)> =
        Vec::with_capacity(state.tls_listeners.len());
    for (lid, handle) in &state.tls_listeners {
        let bucket = buckets.get(lid).cloned().unwrap_or_default();
        if bucket.is_empty() {
            // A listener whose every manual identity vanished would swap
            // in an empty resolver = TLS-disabled. The identity set is
            // frozen at startup, so an empty bucket here means the
            // listener never had manual certs — leave it untouched
            // rather than disable it.
            continue;
        }
        let strict = strict_by_id.get(lid).copied().unwrap_or(false);
        let resolver = SniCertResolver::from_config(&bucket, strict)
            .map_err(|e| anyhow!("rebuild TLS resolver for listener {lid:?}: {e}"))?;
        built.push((handle, Arc::new(resolver)));
    }

    // 5. Atomic swap each (clears the per-listener ServerConfig cache).
    let swapped = built.len();
    for (handle, resolver) in built {
        handle.swap(Some(resolver));
    }

    // 6. Republish the status snapshot. The manual identity SET is
    //    unchanged across a content-only reload, so this is a no-op
    //    refresh today — but it keeps the contract the Codex review
    //    flagged: the watch sender lives with the swapper and is pushed
    //    on every successful swap, so any future cert-derived status
    //    field stays fresh.
    if let Some(tx) = &state.tls_status_tx {
        tx.send_replace(TlsStatusSnapshot::initial(&state.manual_identities));
    }

    Ok(json!({
        "reloaded": true,
        "listeners_swapped": swapped,
        "identities": state
            .manual_identities
            .iter()
            .map(|i| i.server_name.clone())
            .collect::<Vec<_>>(),
    }))
}

fn unix_now() -> Result<UnixTime> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock reports a pre-Unix-epoch time"))?;
    Ok(UnixTime::since_unix_epoch(since))
}

fn to_response(r: Result<JsonValue>) -> (u8, String) {
    match r {
        Ok(v) => (RC_OK, v.to_string()),
        Err(e) => (RC_ERROR, err_body(&e.to_string())),
    }
}

fn err_body(msg: &str) -> String {
    json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    fn install_default_crypto_provider_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    /// A registered-but-empty `webd.listeners` runtime. The bad-cert
    /// test returns at validation before the live strict_sni snapshot,
    /// so an empty namespace (strict defaults false) is sufficient.
    fn empty_listeners_runtime() -> Arc<Runtime> {
        use cosmix_props::PropsRouter;
        use cosmix_props::sqlite::SqliteStore;
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .expect("pragmas");
        let store = Arc::new(SqliteStore::new("webd", conn).expect("store"));
        let mut router = PropsRouter::new("webd");
        let (runtime, _events_rx) =
            listeners_namespace::register_listeners_namespace(&mut router, &store, Vec::new())
                .expect("register listeners namespace");
        runtime
    }

    /// Write a self-signed cert/key pair (NOT an LE chain — fails
    /// `validate_le_chain`, which is what the negative test wants).
    fn write_selfsigned(dir: &Path, name: &str) -> (String, String) {
        let cert =
            rcgen::generate_simple_self_signed(vec![name.to_string()]).expect("rcgen self-signed");
        let cert_path = dir.join(format!("{name}.cert.pem"));
        let key_path = dir.join(format!("{name}.key.pem"));
        std::fs::File::create(&cert_path)
            .unwrap()
            .write_all(cert.cert.pem().as_bytes())
            .unwrap();
        std::fs::File::create(&key_path)
            .unwrap()
            .write_all(cert.key_pair.serialize_pem().as_bytes())
            .unwrap();
        (
            cert_path.to_string_lossy().into_owned(),
            key_path.to_string_lossy().into_owned(),
        )
    }

    fn ident(server_name: &str, cert: &str, key: &str) -> TlsIdentityConfig {
        TlsIdentityConfig {
            server_name: server_name.to_string(),
            cert: cert.to_string(),
            key: key.to_string(),
            default: false,
            no_sni_fallback: false,
        }
    }

    fn one_listener(id: &str, host: &str) -> ResolvedWebdListener {
        ResolvedWebdListener {
            id: id.to_string(),
            bind: "192.0.2.5:443".to_string(),
            external: false,
            enabled: true,
            hosts: vec![host.to_string()],
        }
    }

    /// A reload against a self-signed (non-LE) cert must fail
    /// `validate_le_chain`, return `rc=10`, and leave the prior resolver
    /// untouched (the handle still serves whatever it had).
    #[tokio::test]
    async fn reload_rejects_non_le_cert_and_keeps_prior() {
        install_default_crypto_provider_once();
        let td = TempDir::new().unwrap();
        let (cert, key) = write_selfsigned(td.path(), "site.example");
        // Prior resolver: a sentinel the swap must NOT replace.
        let prior = Arc::new(
            SniCertResolver::from_config(&[ident("site.example", &cert, &key)], false)
                .expect("build prior resolver from the self-signed pair"),
        );
        let handle = ListenerTls::new(Some(prior.clone()));
        let mut listeners = HashMap::new();
        listeners.insert("wg".to_string(), handle.clone());

        let (tx, _rx) = watch::channel(TlsStatusSnapshot::default());
        let state = TlsReloadState::new(
            listeners,
            vec![ident("site.example", &cert, &key)],
            vec![one_listener("wg", "site.example")],
            empty_listeners_runtime(),
            Some(tx),
        );

        let r = handle_reload(&state).await;
        assert!(r.is_err(), "self-signed cert must fail validate_le_chain");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("validate_le_chain"), "got: {msg}");
        // Prior resolver still in the slot (same Arc identity).
        let (cur, _cfg) = handle.current().expect("handle still has a resolver");
        assert!(
            Arc::ptr_eq(&cur, &prior),
            "a failed reload must not swap the live resolver"
        );
    }
}
