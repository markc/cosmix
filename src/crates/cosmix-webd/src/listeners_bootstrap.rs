//! P3 — `bootstrap_upsert_from_config` for the `webd.listeners`
//! namespace: seed the L1 kill-switch/guard rows from the resolved
//! `[[webd.listener]]` config at startup.
//!
//! ## L0-seed / L1-authoritative (differs from `vhosts_bootstrap`)
//!
//! `vhosts_bootstrap` *re-asserts* config-owned fields on every
//! restart — config is authoritative for vhosts. The kill switch is
//! the opposite: config **seeds** a listener's `enabled`/guard state
//! once; thereafter **L1 wins**. A listener an operator killed
//! (`enabled=false`) in a prior session must stay killed across
//! `systemctl restart` — so this path is **upsert-if-absent** for the
//! caller-owned fields:
//!
//! * **absent row** (never seeded, or tombstoned) → seed `enabled` (=
//!   the config bootstrap default) and the daemon-owned `external`
//!   flag.
//! * **present row** → leave `enabled`/guard fields untouched (L1
//!   wins); only refresh the daemon-owned `external` flag if config
//!   changed the listener's interface role (it drives the lockout
//!   guard).
//!
//! Backend-origin (`WriteOrigin::backend()`) writes bypass the
//! `before_set` daemon-owned-column gate but still run the lockout
//! guard (rule 4) and row-shape gate (rule 5), so a config that tries
//! to seed `enabled=false` on a non-external listener fails loudly at
//! startup rather than silently bricking the mesh control path.
//!
//! Orphan rows (a listener dropped from config) are **left in place** —
//! harmless, since the `ListenerSet` only binds ids present in the
//! resolved set.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use cosmix_config::node::ResolvedWebdListener;
use cosmix_props::{
    Actor, MergeMode, PropValue, RecordKey, Runtime, SetOpts, Version, WriteOrigin,
};
use std::sync::Arc;

use crate::listeners_namespace::namespace_name;

/// Seed / reconcile the `webd.listeners` namespace from the resolved
/// listener set. Idempotent; safe to re-run every startup.
pub async fn bootstrap_upsert_from_config(
    runtime: &Arc<Runtime>,
    resolved: &[ResolvedWebdListener],
    ts_ms: i64,
) -> Result<()> {
    let namespace = namespace_name();

    // Snapshot live rows (id → current value) for the present/absent
    // decision. Tombstones are hidden by `list()`; we use the
    // tombstone-aware `version_anchor` for the OCC anchor on write so a
    // re-added listener resurrects rather than colliding on Version(0).
    let snapshot = runtime
        .store()
        .list(&namespace)
        .await
        .context("listeners bootstrap: listing webd.listeners namespace")?;
    let mut existing: BTreeMap<String, PropValue> = BTreeMap::new();
    for record in snapshot.value {
        existing.insert(record.key.key.clone(), record.value);
    }

    for l in resolved {
        let key = RecordKey::collection(namespace.clone(), l.id.clone());
        let expected_version = match runtime.store().version_anchor(&key).await {
            Ok(Some(v)) => v,
            Ok(None) => Version::zero(),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "listeners bootstrap: reading OCC anchor for id={:?}: {e}",
                    l.id
                ));
            }
        };

        let body = match existing.get(&l.id) {
            // Present → L1 is authoritative for enabled/guard. Only
            // refresh the daemon-owned `external` flag if it drifted
            // (a config interface-role change); otherwise skip the
            // write entirely to avoid version churn + a spurious
            // reaction event.
            Some(value) => {
                let live_external = object_bool_field(value, "external");
                if live_external == Some(l.external) {
                    continue;
                }
                let mut obj = BTreeMap::new();
                obj.insert("external".to_string(), PropValue::Bool(l.external));
                PropValue::Object(obj)
            }
            // Absent → seed enabled (config default) + external. Guard
            // fields stay at their schema defaults (unlimited / off).
            // `bound`/`bound_addr`/... are written by the reaction
            // loop's startup writeback.
            None => {
                let mut obj = BTreeMap::new();
                obj.insert("id".to_string(), PropValue::String(l.id.clone()));
                obj.insert("enabled".to_string(), PropValue::Bool(l.enabled));
                obj.insert("external".to_string(), PropValue::Bool(l.external));
                PropValue::Object(obj)
            }
        };

        runtime
            .set_with_origin(
                key,
                body,
                SetOpts {
                    expected_version: Some(expected_version),
                    merge: MergeMode::Patch,
                    actor: Actor::service("webd").expect("valid actor"),
                    cause: Some("listeners_bootstrap".into()),
                    ts_ms,
                },
                WriteOrigin::backend(),
            )
            .await
            .with_context(|| {
                format!(
                    "listeners bootstrap: seeding webd.listeners row id={:?} \
                     (a before_set rejection here means config tried to disable a \
                     non-external listener — the kill switch governs external \
                     listeners only)",
                    l.id
                )
            })?;
    }
    Ok(())
}

/// Read a bool field from a `PropValue::Object`, `None` if absent or
/// not a bool.
fn object_bool_field(value: &PropValue, key: &str) -> Option<bool> {
    match value {
        PropValue::Object(m) => match m.get(key) {
            Some(PropValue::Bool(b)) => Some(*b),
            _ => None,
        },
        _ => None,
    }
}
