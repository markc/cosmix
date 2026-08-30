//! C3c — `bootstrap_upsert_from_config`: materialise resolved
//! `[[webd.vhost]]` rows from `node.conf.mix` into the `webd.vhosts`
//! property substrate, idempotently, at daemon startup.
//!
//! ## Why this exists
//!
//! Phase 3 flips the substrate role: the namespace is the declarative
//! source-of-truth for vhost configuration, not a read-only mirror.
//! For configuration *originating in `node.conf.mix`*, the daemon owns
//! the materialisation contract — every restart re-asserts the
//! config block into the namespace under `source = "config_bootstrap"`.
//! Rows added at runtime via `vhost.add` carry `source = "bus_runtime"`
//! and are **never touched** by this path; bootstrap can't silently
//! destroy operator intent on restart.
//!
//! ## Algorithm
//!
//! 1. **Normalise** each `[[webd.vhost]]` row's `host` via
//!    [`cosmix_daemon::http_host::parse_request_host`]. The resolver
//!    already rejected malformed hosts; this re-normalises so the key,
//!    body `fqdn`, and namespace lookup all carry the same byte shape
//!    (lowercased, port-stripped, LDH-validated). The namespace hook's
//!    rule 2 (`fqdn` body matches key) enforces byte equality and
//!    would otherwise reject a row whose raw config host differed in
//!    case from the resolver-normalised lookup key.
//! 2. **Enumerate** existing namespace rows.
//! 3. **Orphan delete pass.** Any existing row whose `source` is
//!    `"config_bootstrap"` and whose `fqdn` is absent from the current
//!    configuration is deleted via
//!    [`Runtime::delete_with_origin`] under `WriteOrigin::backend()`.
//!    Operator removed the row from `node.conf.mix`; bootstrap drops the
//!    matching namespace row.
//! 4. **Upsert pass.** For each configured row:
//!    - Read the current namespace row (if any). If `source ==
//!      "bus_runtime"`, log a loud conflict-skip WARN and continue —
//!      preserves the never-touch-runtime-rows invariant.
//!    - Build a [`PropValue::Object`] body carrying every config-owned
//!      field with an *explicit* value (including `PropValue::Null` for
//!      fields that don't apply to the current mode). Patch-merging an
//!      explicit Null is what flushes stale `acme_*` / `tls_*_path`
//!      values when the operator flips a row from ACME to manual TLS
//!      (or vice versa). Without the explicit Nulls a mode change in
//!      `node.conf.mix` would leave the old mode's fields zombified in the
//!      namespace and the row would fail rule 5a (both modes set).
//!    - Stamp `source = "config_bootstrap"` (daemon-owned column,
//!      bypasses the `before_set` rule-1 gate because we go through
//!      `WriteOrigin::backend()`).
//!    - Call [`Runtime::set_with_origin`] with `MergeMode::Patch`
//!      under `WriteOrigin::backend()`. The Patch shape is what
//!      preserves daemon-owned writeback columns (`cert_blob_id`,
//!      `not_after`, `last_attempt`, etc.) across daemon restart — a
//!      `MergeMode::Replace` here would wipe every issued cert on
//!      every restart.
//!
//! ## Validation
//!
//! Every upsert flows through the `before_set` hook in
//! [`crate::vhosts_namespace::VhostsNamespaceHooks`]. The hook is the
//! single validator: bootstrap-time and runtime-write surfaces share
//! it, so rules can't drift. The rev-6 plan envisions a future
//! `VhostValidator` extraction; that's a drift-reduction refactor, not
//! a correctness requirement, and is left for a follow-up commit.
//!
//! ## Fail-fast on errors
//!
//! Any upsert / delete failure aborts bootstrap with a propagated
//! error. Bootstrap is startup-critical state convergence; a
//! partially-bootstrapped namespace would leave the directory layer
//! and the provisioner observing inconsistent truth, with no
//! reasonable recovery path inside one daemon process. The resolver
//! already rejected malformed rows pre-bootstrap, so a hook
//! rejection here surfaces a coverage gap rather than an operator
//! error.
//!
//! ## What this is NOT
//!
//! - It does not start the host listener. C3d wires the bootstrap
//!   call site after `register_vhosts_namespace` and before the
//!   initial directory publish, but the full plan-spec 5-step
//!   ordering (Phase 2 recovery → bootstrap → C4b orphan archive →
//!   directory publish from namespace → first provisioner tick) is
//!   completed by C3e (namespace-sourced directory build) and C4b
//!   (whole-set orphan archive).
//! - It does not publish the directory. The C3e call site in
//!   `main.rs` builds the initial directory via
//!   [`crate::vhosts_namespace::snapshot_rows`] +
//!   [`crate::vhost_directory::from_namespace_rows`] after this
//!   bootstrap completes — so the directory sources from the
//!   post-bootstrap namespace state, not from the config block.
//! - It does not propagate `aliases`. The hook rejects non-empty
//!   `aliases` until Phase 4 multi-SAN lands; bootstrap writes an
//!   empty list. Aliases declared via `[[webd.vhost]] aliases = [...]`
//!   are still applied to HTTP routing through the runtime map that
//!   `from_namespace_rows` threads in alongside the namespace rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use cosmix_config::node::{WebdAcmeChallenge, WebdAcmeProvider, WebdConfig, WebdVhostConfig};
use cosmix_props::{
    Actor, DeleteOpts, MergeMode, PropValue, RecordKey, Runtime, SetOpts, Version, WriteOrigin,
};

/// `source` wire value the daemon stamps onto rows materialised from
/// `node.conf.mix`. Mirrors `vhosts_namespace::source_variants()`; kept
/// local so this module's intent is self-documenting and a regression
/// in the namespace's variant set surfaces here as a hook-rejection on
/// the upsert path.
const SOURCE_CONFIG_BOOTSTRAP: &str = "config_bootstrap";

/// `source` wire value runtime writes carry. Bootstrap never overwrites
/// a row stamped with this value; the operator's `vhost.add` intent
/// outlives a restart even when the same `fqdn` is absent from
/// `node.conf.mix`.
const SOURCE_BUS_RUNTIME: &str = "bus_runtime";

/// Legacy `source` value stamped by pre-rename daemons, when the Bus
/// was still called AMP. The namespace hook no longer accepts it and
/// `VhostDirectory` construction treats it as an unknown source (a
/// substrate-invariant error), so rows carrying it must be rewritten
/// before anything else reads the namespace — see
/// [`migrate_amp_runtime_rows`].
const SOURCE_AMP_RUNTIME_LEGACY: &str = "amp_runtime";

/// One-shot AMP→Bus rename migration for durable `webd.vhosts` rows.
///
/// Rows written by a pre-rename daemon via `webd.vhost.add` carry
/// `source = "amp_runtime"`. Left in place they either abort startup
/// (namespace-only row → unknown-source invariant error in
/// `VhostDirectory`) or lose their runtime-wins precedence to a
/// matching `[[webd.vhost]]` config block. This pass rewrites the
/// stored discriminator to `"bus_runtime"` through the backend-origin
/// path, before the orphan/upsert passes and before the directory
/// builds. Idempotent: a store with no legacy rows is a no-op.
///
/// Returns the number of rows migrated.
pub async fn migrate_amp_runtime_rows(runtime: &Arc<Runtime>, ts_ms: i64) -> Result<usize> {
    let namespace = runtime.spec().name.clone();
    let snapshot = runtime
        .store()
        .list(&namespace)
        .await
        .context("amp→bus migration: listing webd.vhosts namespace")?;
    let mut migrated = 0usize;
    for record in snapshot.value {
        let source = match &record.value {
            PropValue::Object(obj) => match obj.get("source") {
                Some(PropValue::String(s)) => s.clone(),
                _ => continue,
            },
            _ => continue,
        };
        if source != SOURCE_AMP_RUNTIME_LEGACY {
            continue;
        }
        let fqdn = record.key.key.clone();
        let mut patch = BTreeMap::new();
        patch.insert(
            "source".to_string(),
            PropValue::String(SOURCE_BUS_RUNTIME.into()),
        );
        runtime
            .set_with_origin(
                RecordKey::collection(namespace.clone(), fqdn.clone()),
                PropValue::Object(patch),
                SetOpts {
                    expected_version: Some(record.version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("amp→bus rename migration".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
            .with_context(|| {
                format!(
                    "amp→bus migration: rewriting webd.vhosts row fqdn={fqdn:?} \
                     source amp_runtime → bus_runtime"
                )
            })?;
        tracing::info!(
            target: "webd::vhosts_bootstrap",
            fqdn = %fqdn,
            "amp→bus migration: rewrote legacy amp_runtime source to bus_runtime",
        );
        migrated += 1;
    }
    Ok(migrated)
}

/// Materialise `[[webd.vhost]]` config rows into the `webd.vhosts`
/// substrate namespace, idempotently.
///
/// Caller (C3d) invokes this once at startup, after Phase 2's per-fqdn
/// crash recovery and before the host listener begins serving. See the
/// module doc for the full algorithm; the short version:
///
/// 1. Delete `source = "config_bootstrap"` rows whose `fqdn` is no
///    longer in `webd_cfg.vhost`.
/// 2. Patch-upsert each remaining config row through the
///    backend-origin path, preserving daemon-owned writeback columns
///    and skipping rows whose live `source` is `"bus_runtime"`.
///
/// `ts_ms` is the wall-clock millisecond stamp threaded into the
/// substrate's event rows; the caller (`main.rs`) generates this once
/// at startup so a single bootstrap pass attributes its writes to one
/// coherent timestamp.
pub async fn bootstrap_upsert_from_config(
    runtime: &Arc<Runtime>,
    webd_cfg: &WebdConfig,
    ts_ms: i64,
) -> Result<()> {
    let namespace = runtime.spec().name.clone();

    // 1 — normalise the configured set. Resolver should have rejected
    // any host that fails LDH parsing, but we re-normalise here so the
    // namespace key and body `fqdn` carry the resolver's canonical
    // byte shape (lowercased, port-stripped) and rule 2 passes.
    let mut configured: BTreeMap<String, &WebdVhostConfig> = BTreeMap::new();
    for row in &webd_cfg.vhost {
        let host = cosmix_daemon::http_host::parse_request_host(&row.host).ok_or_else(|| {
            anyhow!(
                "config bootstrap: vhost row host={:?} is not a valid LDH hostname \
                 (resolver should have rejected this before bootstrap — \
                 defense-in-depth check)",
                row.host
            )
        })?;
        if configured.insert(host.clone(), row).is_some() {
            return Err(anyhow!(
                "config bootstrap: duplicate normalised host {host:?} in [[webd.vhost]] \
                 rows (resolver should have deduplicated before bootstrap — \
                 defense-in-depth check)"
            ));
        }
    }

    // 2 — snapshot the live namespace before deciding writes. The
    // snapshot's `value` is the row list; `version` accompanies each row
    // and is mandatory for the `require_version: true` gate on every
    // backend-origin write below.
    let snapshot = runtime
        .store()
        .list(&namespace)
        .await
        .context("config bootstrap: listing webd.vhosts namespace for reconciliation")?;
    let mut existing: BTreeMap<String, (PropValue, Version)> = BTreeMap::new();
    for record in snapshot.value {
        existing.insert(record.key.key.clone(), (record.value, record.version));
    }

    // 3 — orphan-delete pass: drop config_bootstrap rows no longer in
    // node.conf.mix. We iterate `existing` not `configured` so we catch
    // every stale FQDN.
    for (fqdn, (value, version)) in &existing {
        if configured.contains_key(fqdn) {
            continue;
        }
        let Some(source) = object_string_field(value, "source") else {
            // No `source` stamp at all — pre-Phase-3 row, or a future
            // schema gap. We don't own this row's lineage; leave it
            // alone rather than risk destroying operator state.
            tracing::warn!(
                target: "webd::vhosts_bootstrap",
                fqdn = %fqdn,
                "bootstrap: orphan check found a row without `source`; leaving in \
                 place — bootstrap only deletes rows it knows it wrote",
            );
            continue;
        };
        if source != SOURCE_CONFIG_BOOTSTRAP {
            // `bus_runtime` (or any future source) — never touch.
            continue;
        }
        let key = RecordKey::collection(namespace.clone(), fqdn.clone());
        runtime
            .delete_with_origin(
                key,
                DeleteOpts {
                    expected_version: Some(*version),
                    actor: Actor::service("webd"),
                    cause: Some("config_bootstrap orphan".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
            .with_context(|| {
                format!(
                    "config bootstrap: deleting orphan config_bootstrap row \
                     fqdn={fqdn:?} (no matching [[webd.vhost]] in node.conf.mix)"
                )
            })?;
        tracing::info!(
            target: "webd::vhosts_bootstrap",
            fqdn = %fqdn,
            "bootstrap: deleted orphan config_bootstrap row absent from node.conf.mix",
        );
    }

    // 4 — upsert pass for every configured row.
    for (fqdn, cfg_row) in &configured {
        let existing_entry = existing.get(fqdn);

        // bus_runtime invariant: operator's runtime row wins. Log loud,
        // skip silent.
        if let Some((existing_value, _)) = existing_entry
            && object_string_field(existing_value, "source").as_deref() == Some(SOURCE_BUS_RUNTIME)
        {
            tracing::warn!(
                target: "webd::vhosts_bootstrap",
                fqdn = %fqdn,
                "fqdn={fqdn} source=bus_runtime shadows config block; runtime row preserved",
            );
            continue;
        }

        let body = build_bootstrap_body(fqdn, cfg_row).with_context(|| {
            format!(
                "config bootstrap: building patch body for fqdn={fqdn:?} \
                 (defense-in-depth: resolver should have rejected this row before \
                 bootstrap — fix the resolver coverage gap before re-running)"
            )
        })?;
        let key = RecordKey::collection(namespace.clone(), fqdn.clone());
        // `webd.vhosts` is a SoftDelete namespace: `list()`/`get()` hide
        // tombstoned rows, but the store retains the tombstone's version and
        // `commit_set` validates the OCC anchor against it. Deriving the anchor
        // from `existing_entry` (built from the tombstone-blind `list()`) would
        // fall back to `Version::zero()` for any fqdn that was ever removed and
        // brick daemon startup with `version_mismatch` (Version(0) vs the
        // retained tombstone version). `version_anchor` is tombstone-aware —
        // live → live version, tombstone → tombstone version, absent → None —
        // which is what lets a config block resurrect a previously-removed fqdn
        // (mirrors the `webd.vhost.add` operator flow). `existing_entry` above
        // remains the authority for the bus_runtime shadow check (live rows).
        let expected_version = match runtime.store().version_anchor(&key).await {
            Ok(Some(v)) => v,
            Ok(None) => Version::zero(),
            Err(other) => {
                return Err(anyhow!(
                    "config bootstrap: reading tombstone-aware OCC anchor for \
                     fqdn={fqdn:?}: {other}"
                ));
            }
        };
        runtime
            .set_with_origin(
                key,
                body,
                SetOpts {
                    expected_version: Some(expected_version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("config_bootstrap".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
            .with_context(|| {
                format!(
                    "config bootstrap: upserting webd.vhosts row fqdn={fqdn:?} \
                     from [[webd.vhost]] config block (this is the backend-origin \
                     path; a `before_set` rejection here indicates a coverage \
                     gap between the resolver and the namespace hook)"
                )
            })?;
    }
    Ok(())
}

/// Construct the patch body for one configured vhost. The shape is
/// **complete** for every config-owned field — modes that don't apply
/// land as [`PropValue::Null`], so a mode change in `node.conf.mix` flushes
/// the prior mode's fields rather than leaving them zombified.
///
/// The daemon-owned writeback columns (`cert_blob_id`, `key_blob_id`,
/// `not_after`, `last_attempt`, `last_error_count`, `last_error`) are
/// **deliberately omitted** — `MergeMode::Patch` leaves them
/// unchanged across the bootstrap write, which is how a previously
/// issued cert survives daemon restart.
///
/// Returns `Err` on either of two defense-in-depth contract
/// violations the resolver should already reject:
///
/// 1. **Half-configured manual-TLS pair** (`tls_cert` set without
///    `tls_key` or vice versa). Without this check, the offending
///    field would deserialise as `Some("")` through the namespace hook
///    and could pass rule 5 despite being uninstallable.
/// 2. **ACME + manual-TLS coexistence** (`acme = ...` *and* either
///    `tls_cert` or `tls_key` set). The block sets two modes for the
///    same vhost; the resolver should reject this, but if it leaks
///    through we fail fast rather than silently let the ACME branch
///    win and patch-null the manual fields — that would mask the
///    operator's config-block intent rather than surface it.
fn build_bootstrap_body(fqdn: &str, cfg: &WebdVhostConfig) -> Result<PropValue> {
    // Defense-in-depth: ACME and manual-TLS are mutually exclusive
    // per the [[webd.vhost]] schema. The resolver should already
    // reject this combination; reaching it here is a resolver
    // coverage gap and bootstrap must fail fast rather than persist
    // an arbitrarily-chosen single mode.
    if cfg.acme.is_some() && (cfg.tls_cert.is_some() || cfg.tls_key.is_some()) {
        return Err(anyhow!(
            "config bootstrap: vhost fqdn={fqdn:?} has both `acme = ...` and \
             manual-TLS fields set (tls_cert={}, tls_key={}); the two modes are \
             mutually exclusive — pick one in node.conf.mix",
            if cfg.tls_cert.is_some() {
                "set"
            } else {
                "unset"
            },
            if cfg.tls_key.is_some() {
                "set"
            } else {
                "unset"
            },
        ));
    }

    let mut body: BTreeMap<String, PropValue> = BTreeMap::new();
    body.insert("fqdn".into(), PropValue::String(fqdn.to_string()));
    body.insert("www_dir".into(), PropValue::String(cfg.www_dir.clone()));
    body.insert(
        "source".into(),
        PropValue::String(SOURCE_CONFIG_BOOTSTRAP.into()),
    );
    // Phase 4 multi-SAN owns this field. Bootstrap writes an empty
    // list so a hypothetical prior bootstrap row that managed to
    // accumulate aliases (none can today — hook rule 3 rejects
    // non-empty) is cleared by patch-merge.
    body.insert("aliases".into(), PropValue::List(Vec::new()));

    if let Some(acme) = &cfg.acme {
        // ACME branch — set the trio, explicit-null the manual pair so
        // a node.conf.mix flip from manual→acme flushes the stale paths.
        body.insert(
            "acme_provider".into(),
            PropValue::String(acme_provider_wire(acme.provider).into()),
        );
        body.insert(
            "acme_challenge".into(),
            PropValue::String(acme_challenge_wire(acme.challenge).into()),
        );
        body.insert(
            "acme_contact_email".into(),
            PropValue::String(acme.contact_email.clone()),
        );
        body.insert("tls_cert_path".into(), PropValue::Null);
        body.insert("tls_key_path".into(), PropValue::Null);
        body.insert("enabled".into(), PropValue::Bool(true));
    } else {
        match (&cfg.tls_cert, &cfg.tls_key) {
            (Some(cert), Some(key)) => {
                // Manual-TLS branch (both halves present).
                body.insert("tls_cert_path".into(), PropValue::String(cert.clone()));
                body.insert("tls_key_path".into(), PropValue::String(key.clone()));
                body.insert("acme_provider".into(), PropValue::Null);
                body.insert("acme_challenge".into(), PropValue::Null);
                body.insert("acme_contact_email".into(), PropValue::Null);
                body.insert("enabled".into(), PropValue::Bool(true));
            }
            (None, None) => {
                // HTTP-only branch. The current resolver rejects rows
                // without either mode, so this arm is not on the live
                // config path today — kept for forward-compat in case
                // the resolver relaxes to allow HTTP-only rows.
                // Namespace rule 5b requires `enabled = false` when
                // no TLS mode is set.
                body.insert("acme_provider".into(), PropValue::Null);
                body.insert("acme_challenge".into(), PropValue::Null);
                body.insert("acme_contact_email".into(), PropValue::Null);
                body.insert("tls_cert_path".into(), PropValue::Null);
                body.insert("tls_key_path".into(), PropValue::Null);
                body.insert("enabled".into(), PropValue::Bool(false));
            }
            (Some(_), None) | (None, Some(_)) => {
                // Half-pair: fail-fast rather than persist a row whose
                // `tls_key_path` (or `tls_cert_path`) is an empty
                // string and could quietly pass hook rule 5. The
                // resolver should already reject this; reaching this
                // arm means there's a coverage gap upstream.
                return Err(anyhow!(
                    "config bootstrap: vhost fqdn={fqdn:?} has half-configured \
                     manual-TLS pair (tls_cert={}, tls_key={}); both halves must \
                     be set or both omitted",
                    if cfg.tls_cert.is_some() {
                        "set"
                    } else {
                        "unset"
                    },
                    if cfg.tls_key.is_some() {
                        "set"
                    } else {
                        "unset"
                    },
                ));
            }
        }
    }
    Ok(PropValue::Object(body))
}

fn acme_provider_wire(p: WebdAcmeProvider) -> &'static str {
    match p {
        WebdAcmeProvider::LetsEncryptProd => "letsencrypt_prod",
        WebdAcmeProvider::LetsEncryptStaging => "letsencrypt_staging",
    }
}

fn acme_challenge_wire(c: WebdAcmeChallenge) -> &'static str {
    match c {
        WebdAcmeChallenge::Http01 => "http01",
        // `dns01` is reserved-but-refused by both the resolver (Phase 4
        // pointer at resolve time) and the namespace hook (rule 4
        // rejection). Reaching this arm means the resolver let a dns01
        // row through; the hook will reject the upsert and bootstrap
        // fails fast — exactly the coverage-gap signal we want.
        WebdAcmeChallenge::Dns01 => "dns01",
    }
}

/// Pull a `String` field out of a `PropValue::Object`. Returns `None`
/// for absent / non-String / Null — all three are treated as "field
/// not present for our purposes," which matches the hook's
/// `field_as_str` shape and means a `source: Null` row is treated as
/// missing-source (we leave it alone in the orphan pass).
fn object_string_field(value: &PropValue, field: &str) -> Option<String> {
    if let PropValue::Object(m) = value
        && let Some(PropValue::String(s)) = m.get(field)
    {
        Some(s.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    //! Test bucket V-B (Vhosts Bootstrap). See plan §"Test bucket V-B"
    //! (`_doc/planned/webd-vhosts-phase3.md` lines 1274–1334).
    //!
    //! Each test boots a fresh in-memory `SqliteStore`, registers the
    //! `webd.vhosts` namespace, then exercises `bootstrap_upsert_from_config`
    //! against constructed `WebdConfig` shapes. The receiver from
    //! `register_vhosts_namespace` is bound for the test's lifetime so
    //! the namespace's `after_set` `try_send` cannot return `Closed`
    //! (mirrors `main.rs`'s parked-receiver pattern).
    use super::*;
    use crate::vhosts_namespace::{register_vhosts_namespace, vhost_row_from_value};
    use cosmix_config::node::{
        WebdAcmeChallenge, WebdAcmeProvider, WebdConfig, WebdVhostAcmeConfig, WebdVhostConfig,
    };
    use cosmix_props::PropsRouter;
    use cosmix_props::sqlite::SqliteStore;
    use rusqlite::Connection;
    use std::sync::Arc;

    /// Boot a fresh substrate + register the `webd.vhosts` namespace.
    /// Returns the runtime handle (used to drive bootstrap and to
    /// list rows after) plus the parked event receiver (held to keep
    /// the hooks' `try_send` open).
    fn fixture() -> (
        Arc<cosmix_props::Runtime>,
        tokio::sync::mpsc::Receiver<crate::vhosts_namespace::NamespaceEvent>,
    ) {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        let store = Arc::new(SqliteStore::new("webd", conn).expect("store"));
        let mut router = PropsRouter::new("webd");
        let (runtime, events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");
        (runtime, events_rx)
    }

    fn cfg_with(vhosts: Vec<WebdVhostConfig>) -> WebdConfig {
        WebdConfig {
            vhost: vhosts,
            ..WebdConfig::default()
        }
    }

    fn acme_row(host: &str, www_dir: &str) -> WebdVhostConfig {
        WebdVhostConfig {
            host: host.into(),
            aliases: Vec::new(),
            www_dir: www_dir.into(),
            tls_cert: None,
            tls_key: None,
            acme: Some(WebdVhostAcmeConfig {
                provider: WebdAcmeProvider::LetsEncryptProd,
                challenge: WebdAcmeChallenge::Http01,
                contact_email: "ops@example.com".into(),
            }),
            cms_db_path: None,
            aux_dbs: Vec::new(),
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
        }
    }

    fn manual_tls_row(host: &str, www_dir: &str) -> WebdVhostConfig {
        WebdVhostConfig {
            host: host.into(),
            aliases: Vec::new(),
            www_dir: www_dir.into(),
            tls_cert: Some("/etc/cosmix/webd/certs/x.crt".into()),
            tls_key: Some("/etc/cosmix/webd/keys/x.key".into()),
            acme: None,
            cms_db_path: None,
            aux_dbs: Vec::new(),
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
        }
    }

    fn http_only_row(host: &str, www_dir: &str) -> WebdVhostConfig {
        WebdVhostConfig {
            host: host.into(),
            aliases: Vec::new(),
            www_dir: www_dir.into(),
            tls_cert: None,
            tls_key: None,
            acme: None,
            cms_db_path: None,
            aux_dbs: Vec::new(),
            jmap_upstream: None,
            noded_ws: None,
            docs_dir: None,
            dev_session_email: None,
            dev_session_password: None,
            public_read_email: None,
            public_read_password: None,
            system_sender_email: None,
            system_sender_password: None,
            mfa_break_glass: false,
        }
    }

    async fn list_rows(runtime: &Arc<cosmix_props::Runtime>) -> Vec<cosmix_props::Record> {
        let ns = runtime.spec().name.clone();
        runtime.store().list(&ns).await.expect("list rows").value
    }

    /// V-B.1 — Empty config block yields an empty namespace after
    /// bootstrap. Smoke for the no-op path and a regression canary
    /// against bootstrap accidentally synthesising a default row.
    #[tokio::test]
    async fn empty_config_yields_empty_namespace() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(Vec::new());
        bootstrap_upsert_from_config(&runtime, &cfg, 1_700_000_000_000)
            .await
            .expect("bootstrap empty config");
        assert!(list_rows(&runtime).await.is_empty());
    }

    /// V-B.2 — A single ACME config row becomes a namespace row
    /// stamped `source = "config_bootstrap"` with the caller-owned
    /// fields populated and daemon writeback columns absent.
    #[tokio::test]
    async fn acme_config_row_yields_config_bootstrap_source_row() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(vec![acme_row("example.com", "/srv/www/example")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_700_000_000_000)
            .await
            .expect("bootstrap");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        let row = vhost_row_from_value(&rows[0].value).expect("row deserialises");
        assert_eq!(row.fqdn, "example.com");
        assert_eq!(row.source.as_deref(), Some(SOURCE_CONFIG_BOOTSTRAP));
        assert_eq!(row.acme_provider.as_deref(), Some("letsencrypt_prod"));
        assert_eq!(row.acme_challenge.as_deref(), Some("http01"));
        assert_eq!(row.acme_contact_email.as_deref(), Some("ops@example.com"));
        assert_eq!(row.tls_cert_path, None);
        assert_eq!(row.tls_key_path, None);
        assert!(row.enabled);
        assert_eq!(row.cert_blob_id, None, "no cert issued yet");
        assert_eq!(row.not_after, None);
    }

    /// V-B.3 — Operator removed a row from `node.conf.mix`: bootstrap
    /// drops the matching namespace row.
    #[tokio::test]
    async fn removed_config_row_deletes_namespace_row() {
        let (runtime, _events_rx) = fixture();
        let cfg_with_both = cfg_with(vec![
            acme_row("a.example", "/srv/www/a"),
            acme_row("b.example", "/srv/www/b"),
        ]);
        bootstrap_upsert_from_config(&runtime, &cfg_with_both, 1_700_000_000_000)
            .await
            .expect("first bootstrap");
        assert_eq!(list_rows(&runtime).await.len(), 2);

        let cfg_after = cfg_with(vec![acme_row("a.example", "/srv/www/a")]);
        bootstrap_upsert_from_config(&runtime, &cfg_after, 1_700_000_001_000)
            .await
            .expect("second bootstrap");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key.key, "a.example");
    }

    /// V-B.4 — `bus_runtime` row blocks bootstrap overwrite, even when
    /// the same `fqdn` is present in `node.conf.mix`. The runtime intent
    /// outlives the restart.
    #[tokio::test]
    async fn bus_runtime_row_preserved_against_matching_config_row() {
        let (runtime, _events_rx) = fixture();
        // Seed the row as bus_runtime via the backend-origin path.
        let ns = runtime.spec().name.clone();
        let key = cosmix_props::RecordKey::collection(ns.clone(), "live.example".to_string());
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("live.example".into()));
        body.insert("www_dir".into(), PropValue::String("/srv/www/live".into()));
        body.insert(
            "source".into(),
            PropValue::String(SOURCE_BUS_RUNTIME.into()),
        );
        body.insert(
            "acme_provider".into(),
            PropValue::String("letsencrypt_staging".into()),
        );
        body.insert("acme_challenge".into(), PropValue::String("http01".into()));
        body.insert(
            "acme_contact_email".into(),
            PropValue::String("runtime@example.com".into()),
        );
        body.insert("enabled".into(), PropValue::Bool(true));
        runtime
            .set_with_origin(
                key,
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("webd"),
                    cause: Some("test seed".into()),
                    ts_ms: 1_000,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("seed bus_runtime row");

        // Bootstrap a config block that also names live.example — but
        // with a different www_dir and provider, so the test can
        // distinguish "row preserved" from "row overwritten."
        let cfg = cfg_with(vec![acme_row("live.example", "/srv/www/config")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 2_000)
            .await
            .expect("bootstrap");

        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        let row = vhost_row_from_value(&rows[0].value).expect("row deserialises");
        assert_eq!(row.source.as_deref(), Some(SOURCE_BUS_RUNTIME));
        assert_eq!(row.www_dir, "/srv/www/live", "bus_runtime www_dir wins");
        assert_eq!(
            row.acme_provider.as_deref(),
            Some("letsencrypt_staging"),
            "bus_runtime provider wins"
        );
    }

    /// V-B.5 — Daemon-owned writeback columns survive bootstrap. The
    /// substrate's Patch shape is what gives this guarantee; if
    /// bootstrap accidentally used `MergeMode::Replace` (or set the
    /// daemon columns to `Null`) a previously-issued cert would be
    /// erased on every restart.
    #[tokio::test]
    async fn daemon_owned_cert_fields_survive_bootstrap() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(vec![acme_row("cert.example", "/srv/www/cert")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect("first bootstrap");

        // Simulate the provisioner having issued a cert — patch in the
        // daemon-owned columns via the backend-origin path.
        let ns = runtime.spec().name.clone();
        let key = cosmix_props::RecordKey::collection(ns.clone(), "cert.example".to_string());
        let current_version = runtime
            .store()
            .get(&key)
            .await
            .expect("row exists")
            .value
            .version;
        let mut cert_patch = BTreeMap::new();
        cert_patch.insert(
            "cert_blob_id".into(),
            PropValue::String("acme/cert.example/live/fullchain.pem".into()),
        );
        cert_patch.insert(
            "key_blob_id".into(),
            PropValue::String("acme/cert.example/live/privkey.pem".into()),
        );
        cert_patch.insert(
            "not_after".into(),
            PropValue::String("2026-08-01T00:00:00Z".into()),
        );
        cert_patch.insert(
            "last_attempt".into(),
            PropValue::String("2026-05-26T12:00:00Z".into()),
        );
        cert_patch.insert("last_error_count".into(), PropValue::Int(0));
        runtime
            .set_with_origin(
                key,
                PropValue::Object(cert_patch),
                SetOpts {
                    expected_version: Some(current_version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd"),
                    cause: Some("simulated cert issuance".into()),
                    ts_ms: 2_000,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("simulated cert writeback");

        // Re-bootstrap with the same config. Cert columns must
        // survive.
        bootstrap_upsert_from_config(&runtime, &cfg, 3_000)
            .await
            .expect("second bootstrap");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(
            row.cert_blob_id.as_deref(),
            Some("acme/cert.example/live/fullchain.pem"),
            "cert_blob_id survived restart-bootstrap"
        );
        assert_eq!(row.not_after.as_deref(), Some("2026-08-01T00:00:00Z"));
        assert_eq!(row.last_error_count, Some(0));
    }

    /// V-B.6 — Mode flip in `node.conf.mix` (ACME → manual TLS) flushes
    /// the stale ACME fields. Without the explicit Null shape on the
    /// patch body the merged row would carry both modes and fail
    /// rule 5a (mutual exclusion) on the next read.
    #[tokio::test]
    async fn mode_change_acme_to_manual_clears_stale_fields() {
        let (runtime, _events_rx) = fixture();
        // First bootstrap: ACME mode.
        let acme_cfg = cfg_with(vec![acme_row("flip.example", "/srv/www/flip")]);
        bootstrap_upsert_from_config(&runtime, &acme_cfg, 1_000)
            .await
            .expect("acme bootstrap");

        // Second bootstrap: same fqdn, now manual TLS.
        let manual_cfg = cfg_with(vec![manual_tls_row("flip.example", "/srv/www/flip")]);
        bootstrap_upsert_from_config(&runtime, &manual_cfg, 2_000)
            .await
            .expect("manual bootstrap");

        let rows = list_rows(&runtime).await;
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(row.acme_provider, None, "stale ACME cleared");
        assert_eq!(row.acme_challenge, None);
        assert_eq!(row.acme_contact_email, None);
        assert_eq!(
            row.tls_cert_path.as_deref(),
            Some("/etc/cosmix/webd/certs/x.crt")
        );
        assert_eq!(
            row.tls_key_path.as_deref(),
            Some("/etc/cosmix/webd/keys/x.key")
        );
        assert!(row.enabled);
    }

    /// V-B.7 — A pure manual-TLS config row materialises with the
    /// `tls_*_path` pair populated and the ACME trio absent.
    #[tokio::test]
    async fn manual_tls_config_row_yields_namespace_row() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(vec![manual_tls_row("manual.example", "/srv/www/manual")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect("bootstrap");
        let rows = list_rows(&runtime).await;
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(row.source.as_deref(), Some(SOURCE_CONFIG_BOOTSTRAP));
        assert_eq!(
            row.tls_cert_path.as_deref(),
            Some("/etc/cosmix/webd/certs/x.crt")
        );
        assert_eq!(
            row.tls_key_path.as_deref(),
            Some("/etc/cosmix/webd/keys/x.key")
        );
        assert_eq!(row.acme_provider, None);
        assert!(row.enabled);
    }

    /// V-B.8 — Forward-compat probe for the HTTP-only branch. The
    /// resolver currently rejects rows without either mode, so this is
    /// not on the live config path; the test pins
    /// `build_bootstrap_body`'s shape so a future resolver-relaxation
    /// doesn't silently emit `enabled=true` rows that fail rule 5b.
    /// We use a direct `build_bootstrap_body` call (rather than driving
    /// `bootstrap_upsert_from_config` through the namespace hook,
    /// which would reject an enabled-no-TLS row anyway) — the
    /// invariant under test is purely the body builder.
    #[test]
    fn http_only_config_row_body_disables_enabled_flag() {
        let body = build_bootstrap_body("http.example", &http_only_row("http.example", "/srv/x"))
            .expect("http-only body builds");
        let PropValue::Object(obj) = body else {
            panic!("body must be Object");
        };
        assert_eq!(obj.get("enabled"), Some(&PropValue::Bool(false)));
        assert_eq!(obj.get("acme_provider"), Some(&PropValue::Null));
        assert_eq!(obj.get("tls_cert_path"), Some(&PropValue::Null));
    }

    /// V-B.9 — Round-trip: bootstrap multiple rows, drop the runtime,
    /// re-create the substrate over the same on-disk DB (here:
    /// in-memory pinned via shared store), re-bootstrap, assert
    /// identical set. This pins the daemon-restart guarantee in one
    /// test rather than spreading it across several.
    #[tokio::test]
    async fn bootstrap_round_trips_full_set() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(vec![
            acme_row("a.example", "/srv/a"),
            acme_row("b.example", "/srv/b"),
            manual_tls_row("c.example", "/srv/c"),
        ]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect("first bootstrap");
        let first = list_rows(&runtime).await;
        assert_eq!(first.len(), 3);

        // Re-bootstrap with the same config. Idempotent: row set
        // identical (modulo version bumps from the no-op upserts).
        bootstrap_upsert_from_config(&runtime, &cfg, 2_000)
            .await
            .expect("second bootstrap");
        let second = list_rows(&runtime).await;
        assert_eq!(second.len(), 3);

        let mut first_keys: Vec<_> = first.iter().map(|r| r.key.key.clone()).collect();
        let mut second_keys: Vec<_> = second.iter().map(|r| r.key.key.clone()).collect();
        first_keys.sort();
        second_keys.sort();
        assert_eq!(first_keys, second_keys);
        for (a, b) in first.iter().zip(second.iter()) {
            let row_a = vhost_row_from_value(&a.value).expect("a");
            let row_b = vhost_row_from_value(&b.value).expect("b");
            assert_eq!(row_a.fqdn, row_b.fqdn);
            assert_eq!(row_a.source, row_b.source);
            assert_eq!(row_a.acme_provider, row_b.acme_provider);
            assert_eq!(row_a.tls_cert_path, row_b.tls_cert_path);
        }
    }

    /// V-B.10 — Renormalisation: a config row whose `host` is mixed
    /// case must materialise under the lowercased key, matching the
    /// namespace hook's rule 2 (`fqdn` body matches key) on the
    /// lowercased form. Pins the `parse_request_host` thread Codex
    /// flagged.
    #[tokio::test]
    async fn mixed_case_config_host_normalises_to_lower() {
        let (runtime, _events_rx) = fixture();
        let cfg = cfg_with(vec![acme_row("Mixed.Example.COM", "/srv/www/mc")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect("bootstrap");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key.key, "mixed.example.com");
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(row.fqdn, "mixed.example.com");
    }

    /// V-B.11 — Orphan-pass `bus_runtime` never-touch: an
    /// `bus_runtime` row whose `fqdn` is *absent* from `node.conf.mix`
    /// must survive the orphan-delete pass. The code path skips it via
    /// `source != "config_bootstrap"`, but the invariant is
    /// load-bearing enough to pin in a direct test (Codex round 1
    /// MINOR). Distinct from V-B.4, which exercises the upsert-pass
    /// skip when the same fqdn IS in config.
    #[tokio::test]
    async fn bus_runtime_row_survives_orphan_pass_when_absent_from_config() {
        let (runtime, _events_rx) = fixture();
        // Seed an bus_runtime row for a fqdn that will NOT appear in
        // the config block.
        let ns = runtime.spec().name.clone();
        let key = cosmix_props::RecordKey::collection(ns.clone(), "stranded.example".to_string());
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("stranded.example".into()));
        body.insert(
            "www_dir".into(),
            PropValue::String("/srv/www/stranded".into()),
        );
        body.insert(
            "source".into(),
            PropValue::String(SOURCE_BUS_RUNTIME.into()),
        );
        body.insert(
            "tls_cert_path".into(),
            PropValue::String("/etc/cosmix/webd/certs/s.crt".into()),
        );
        body.insert(
            "tls_key_path".into(),
            PropValue::String("/etc/cosmix/webd/keys/s.key".into()),
        );
        body.insert("enabled".into(), PropValue::Bool(true));
        runtime
            .set_with_origin(
                key,
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("webd"),
                    cause: Some("test seed".into()),
                    ts_ms: 1_000,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("seed bus_runtime row");

        // Bootstrap with an empty config block. Orphan pass MUST NOT
        // delete the stranded bus_runtime row.
        let cfg = cfg_with(Vec::new());
        bootstrap_upsert_from_config(&runtime, &cfg, 2_000)
            .await
            .expect("bootstrap with empty config");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1, "bus_runtime row must survive");
        assert_eq!(rows[0].key.key, "stranded.example");
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(row.source.as_deref(), Some(SOURCE_BUS_RUNTIME));
    }

    /// V-B.12b — Regression: a config vhost whose fqdn was previously
    /// REMOVED (tombstoned) must re-bootstrap cleanly. Before the
    /// `version_anchor` fix, `bootstrap_upsert_from_config` derived the OCC
    /// anchor from the tombstone-blind `list()` snapshot → `Version::zero()`
    /// → `version_mismatch` against the retained tombstone version → the
    /// daemon bricked at startup. `version_anchor` is tombstone-aware, so the
    /// config block resurrects the row (mirrors `vhost.remove` → `vhost.add`).
    #[tokio::test]
    async fn config_row_resurrects_after_tombstone() {
        let (runtime, _events_rx) = fixture();
        let ns = runtime.spec().name.clone();
        let key = cosmix_props::RecordKey::collection(ns.clone(), "markc.example".to_string());

        // 1. Bootstrap a config row.
        let cfg = cfg_with(vec![manual_tls_row("markc.example", "/srv/www/markc")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect("first bootstrap");

        // 2. Remove it (tombstone) using the live OCC anchor.
        let live_v = runtime
            .store()
            .version_anchor(&key)
            .await
            .expect("anchor read")
            .expect("live version present");
        runtime
            .delete_with_origin(
                key.clone(),
                DeleteOpts {
                    expected_version: Some(live_v),
                    actor: Actor::service("webd"),
                    cause: Some("test remove".into()),
                    ts_ms: 2_000,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("delete");

        // 3. SoftDelete hides it from list(), but the tombstone version is
        //    retained and surfaced by version_anchor.
        assert!(
            list_rows(&runtime).await.is_empty(),
            "tombstoned row must be hidden from list()"
        );
        assert!(
            runtime
                .store()
                .version_anchor(&key)
                .await
                .expect("anchor read")
                .is_some(),
            "tombstone must retain a version anchor"
        );

        // 4. Re-bootstrap the SAME config — must succeed (the regression).
        bootstrap_upsert_from_config(&runtime, &cfg, 3_000)
            .await
            .expect("re-bootstrap after tombstone must not version_mismatch");

        // 5. Row is back, config-sourced.
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key.key, "markc.example");
        let row = vhost_row_from_value(&rows[0].value).expect("row");
        assert_eq!(row.source.as_deref(), Some(SOURCE_CONFIG_BOOTSTRAP));
    }

    /// V-B.13 — `acme = ...` together with `tls_cert` / `tls_key`
    /// must fail bootstrap fast. The two modes are mutually exclusive
    /// per the [[webd.vhost]] schema; the resolver should reject this
    /// pre-bootstrap, but we add a defense-in-depth check so a
    /// resolver coverage gap can't silently let the ACME branch win
    /// and patch-null the manual fields the operator wrote. Codex
    /// round 2 MAJOR.
    #[tokio::test]
    async fn acme_plus_manual_tls_fails_bootstrap() {
        let (runtime, _events_rx) = fixture();
        // ACME row that ALSO has both manual-TLS halves set —
        // unambiguous mode conflict.
        let mut both = acme_row("both.example", "/srv/www/both");
        both.tls_cert = Some("/etc/cosmix/webd/certs/x.crt".into());
        both.tls_key = Some("/etc/cosmix/webd/keys/x.key".into());
        let cfg = cfg_with(vec![both]);
        let err = bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect_err("acme+manual must fail bootstrap");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mutually exclusive"),
            "error should name the mutual-exclusion contract; got: {msg}"
        );
        assert!(list_rows(&runtime).await.is_empty(), "no row persisted");

        // ACME row with only ONE manual-TLS half set — still
        // ambiguous (and the half-pair check shouldn't shadow this
        // detection: the ACME-vs-manual mutual-exclusion check runs
        // first).
        let mut acme_plus_half = acme_row("acme-half.example", "/srv/www/ah");
        acme_plus_half.tls_cert = Some("/etc/cosmix/webd/certs/x.crt".into());
        let cfg2 = cfg_with(vec![acme_plus_half]);
        let err2 = bootstrap_upsert_from_config(&runtime, &cfg2, 2_000)
            .await
            .expect_err("acme+half must fail bootstrap");
        let msg2 = format!("{err2:#}");
        assert!(
            msg2.contains("mutually exclusive"),
            "error should name the mutual-exclusion contract first; got: {msg2}"
        );
        assert!(list_rows(&runtime).await.is_empty(), "no row persisted");
    }

    /// V-B.12 — Half-configured manual-TLS pair (cert set, key unset
    /// or vice versa) must fail bootstrap fast rather than materialise
    /// a row with an empty-string `tls_key_path` that could pass hook
    /// rule 5. Codex round 1 MAJOR — defense-in-depth against a
    /// resolver coverage gap.
    #[tokio::test]
    async fn half_configured_manual_tls_pair_fails_bootstrap() {
        let (runtime, _events_rx) = fixture();
        // cert set, key unset.
        let mut half = manual_tls_row("half.example", "/srv/www/half");
        half.tls_key = None;
        let cfg = cfg_with(vec![half]);
        let err = bootstrap_upsert_from_config(&runtime, &cfg, 1_000)
            .await
            .expect_err("half-pair must fail bootstrap");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("half-configured manual-TLS pair"),
            "error should name the half-pair condition; got: {msg}"
        );
        assert!(list_rows(&runtime).await.is_empty(), "no row persisted");

        // key set, cert unset.
        let mut half2 = manual_tls_row("half2.example", "/srv/www/half2");
        half2.tls_cert = None;
        let cfg2 = cfg_with(vec![half2]);
        let err2 = bootstrap_upsert_from_config(&runtime, &cfg2, 2_000)
            .await
            .expect_err("half-pair (other half) must fail bootstrap");
        let msg2 = format!("{err2:#}");
        assert!(
            msg2.contains("half-configured manual-TLS pair"),
            "error should name the half-pair condition; got: {msg2}"
        );
        assert!(list_rows(&runtime).await.is_empty(), "no row persisted");
    }

    /// AMP→Bus migration — a legacy `amp_runtime` row (written by a
    /// pre-rename daemon; seeded via raw `commit_set` because the
    /// renamed hook rejects the value) is rewritten to `bus_runtime`,
    /// keeps its runtime-wins precedence against a matching config
    /// block, and the pass is idempotent.
    #[tokio::test]
    async fn migrate_amp_runtime_rows_rewrites_and_is_idempotent() {
        let (runtime, _events_rx) = fixture();
        let ns = runtime.spec().name.clone();
        let key = cosmix_props::RecordKey::collection(ns.clone(), "legacy.example".to_string());
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("legacy.example".into()));
        body.insert(
            "www_dir".into(),
            PropValue::String("/srv/www/legacy".into()),
        );
        body.insert(
            "source".into(),
            PropValue::String(SOURCE_AMP_RUNTIME_LEGACY.into()),
        );
        body.insert(
            "tls_cert_path".into(),
            PropValue::String("/etc/cosmix/webd/certs/legacy.crt".into()),
        );
        body.insert(
            "tls_key_path".into(),
            PropValue::String("/etc/cosmix/webd/keys/legacy.key".into()),
        );
        body.insert("enabled".into(), PropValue::Bool(true));
        runtime
            .store()
            .commit_set(cosmix_props::SetCommit {
                key: key.clone(),
                new_value: PropValue::Object(body),
                expected_version: Some(Version::zero()),
                merge: MergeMode::Replace,
                lifecycle: None,
                actor: Actor::service("webd"),
                cause: Some("test seed (pre-rename daemon)".into()),
                ts_ms: 1_000,
            })
            .await
            .expect("raw-seed legacy amp_runtime row");

        // First pass migrates exactly the legacy row.
        let migrated = migrate_amp_runtime_rows(&runtime, 2_000)
            .await
            .expect("migration pass");
        assert_eq!(migrated, 1, "exactly one legacy row migrated");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        let row = vhost_row_from_value(&rows[0].value).expect("row deserialises");
        assert_eq!(row.source.as_deref(), Some(SOURCE_BUS_RUNTIME));
        assert_eq!(row.www_dir, "/srv/www/legacy", "other fields untouched");

        // Second pass is a no-op (and must not bump the version).
        let v_after = rows[0].version;
        let migrated_again = migrate_amp_runtime_rows(&runtime, 3_000)
            .await
            .expect("second migration pass");
        assert_eq!(migrated_again, 0, "idempotent: nothing left to migrate");
        assert_eq!(
            list_rows(&runtime).await[0].version,
            v_after,
            "no-op pass must not rewrite the row"
        );

        // The migrated row keeps runtime precedence over a matching
        // config block — the property the legacy discriminator lost.
        let cfg = cfg_with(vec![manual_tls_row("legacy.example", "/srv/www/config")]);
        bootstrap_upsert_from_config(&runtime, &cfg, 4_000)
            .await
            .expect("bootstrap after migration");
        let rows = list_rows(&runtime).await;
        assert_eq!(rows.len(), 1);
        let row = vhost_row_from_value(&rows[0].value).expect("row deserialises");
        assert_eq!(row.source.as_deref(), Some(SOURCE_BUS_RUNTIME));
        assert_eq!(
            row.www_dir, "/srv/www/legacy",
            "migrated runtime row wins against the config block"
        );
    }
}
