//! `maild.tls.reload` Bus verb — Phase 5a-commit-2 hot-swap entry
//! point. Walks the substrate's `maild.domains` rows, merges every
//! row's `tls_identity_name` with the **startup-snapshot**
//! `[[tls.identity]]` config rows (config wins on `server_name`
//! collision; collision surfaces as a response warning), rebuilds a
//! fresh [`SniCertResolver`], re-projects the read-only
//! `maild.tls_identities` namespace from the merged set, and atomically
//! swaps the resolver into the shared [`TlsSlot`] and clears the
//! [`ServerConfigCache`]. The next accepted connection sees the new
//! resolver; in-flight handshakes drain on the captured prior
//! `Arc<ServerConfig>`.
//!
//! The verb is daemon-global (no per-identity argument). A single
//! `tokio::sync::Mutex` inside [`TlsReloadState`] serializes concurrent
//! reload calls so the read-rebuild-reproject-swap-clear-cache sequence
//! is never interleaved. Concurrent reloads reduce to "last writer
//! wins" on the swap, which is correct semantics for a refresh
//! primitive.
//!
//! Failure posture: fail-loud-and-keep. If any substrate-declared
//! identity fails (missing PEM, canonical-path escape, cert parse
//! error), the verb returns `rc=10` with a per-identity error list and
//! the prior resolver keeps serving — no half-valid swap. The resolver
//! build itself happens BEFORE the swap, so even a builder failure
//! (key↔cert mismatch surfaced by `SniCertResolver::from_config`)
//! leaves the slot untouched.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use cosmix_client::IncomingCommand;
use cosmix_props::runtime::Runtime;
use cosmix_props::value::PropValue;
use serde_json::{Value as JsonValue, json};
use tokio::sync::Mutex;

use crate::config::TlsIdentityConfig;
use crate::props::tls_identities::IdentitySource;
use crate::props::{domains, tls_identities};
use crate::tls::{ServerConfigCache, SniCertResolver, TlsSlot};

const RC_OK: u8 = 0;
const RC_ERROR: u8 = 10;

/// Shared state threaded into `bus::run` for the `maild.tls.*` verb
/// surface. Clone-cheap (every field is already `Arc`-shaped).
#[derive(Clone)]
pub struct TlsReloadState {
    /// Live resolver slot. The reload `store`s a freshly-built resolver
    /// here once the merge + reproject succeed.
    pub tls_slot: TlsSlot,
    /// Shared `Arc<ServerConfig>` cache. Cleared immediately after
    /// `tls_slot.store(...)` so the next accept-time `load_full` builds
    /// a fresh `ServerConfig` keyed on the new resolver Arc.
    pub tls_config_cache: Arc<ServerConfigCache>,
    /// Startup snapshot of `[[tls.identity]]` config rows. Captured at
    /// daemon init and never re-parsed — operators who want to change
    /// the config-side identity set still need a daemon restart. The
    /// substrate-side row path is what 5a makes hot-swappable.
    pub baseline: Arc<Vec<TlsIdentityConfig>>,
    /// `maild.domains` runtime — the reload walks every row and reads
    /// `tls_identity_name` to derive the substrate-declared identity
    /// set.
    pub domains_runtime: Arc<Runtime>,
    /// `maild.tls_identities` runtime — the reload re-projects the
    /// merged identity list into this namespace BEFORE the resolver
    /// swap so subscribers see identity rows update before cert
    /// behaviour changes.
    pub tls_identities_runtime: Arc<Runtime>,
    /// PEM root for substrate-declared identities. Default is
    /// [`crate::config::default_tls_key_root`] (`/var/lib/cosmix/maild/tls/`).
    pub tls_root: PathBuf,
    /// Resolver-global strict-SNI flag — mirrored from `TlsConfig`.
    pub strict_sni: bool,
    /// Serializes concurrent reloads. The verb body holds this for the
    /// whole (walk-rebuild-reproject-swap-clear-cache) sequence.
    pub reload_lock: Arc<Mutex<()>>,
}

impl TlsReloadState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tls_slot: TlsSlot,
        tls_config_cache: Arc<ServerConfigCache>,
        baseline: Arc<Vec<TlsIdentityConfig>>,
        domains_runtime: Arc<Runtime>,
        tls_identities_runtime: Arc<Runtime>,
        tls_root: PathBuf,
        strict_sni: bool,
    ) -> Self {
        Self {
            tls_slot,
            tls_config_cache,
            baseline,
            domains_runtime,
            tls_identities_runtime,
            tls_root,
            strict_sni,
            reload_lock: Arc::new(Mutex::new(())),
        }
    }
}

/// Dispatch a `maild.tls.*` action. Returns `(rc, body_json)`.
pub async fn dispatch(action: &str, cmd: &IncomingCommand, state: &TlsReloadState) -> (u8, String) {
    match action {
        "reload" => to_response(handle_reload(cmd, state).await),
        other => (RC_ERROR, err_body(&format!("unknown tls action: {other}"))),
    }
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

/// Read every `maild.domains` row and extract the substrate-declared
/// identity set: each row whose `tls_identity_name` is a non-null
/// string contributes one identity. Validation already ran on the
/// substrate write path (see [`domains::validate_tls_identity_name`]);
/// we re-validate here as defence-in-depth in case a row predates the
/// 5a-commit-2 tightening or future regressions slip past the hook.
async fn collect_substrate_identities(
    domains_runtime: &Arc<Runtime>,
    tls_root: &Path,
) -> Result<Vec<TlsIdentityConfig>> {
    let snap = domains_runtime
        .store()
        .list(&domains::namespace_name())
        .await
        .map_err(|e| anyhow!("list maild.domains for tls reload: {e}"))?;

    let mut out: Vec<TlsIdentityConfig> = Vec::new();
    for record in snap.value {
        let name = match &record.value {
            PropValue::Object(m) => match m.get("tls_identity_name") {
                Some(PropValue::String(s)) if !s.is_empty() => s.clone(),
                _ => continue,
            },
            _ => continue,
        };
        // Defence-in-depth re-validation; the substrate write path
        // already enforced this, but we never want a path-traversal
        // value to reach `<tls_root>.join(...)` even if a future
        // regression slips one past the hook.
        domains::validate_tls_identity_name(&name).map_err(|e| {
            anyhow!(
                "domain row {:?}: invalid tls_identity_name {name:?}: {e}",
                record.key.key
            )
        })?;

        let (cert_path, key_path) = canonical_pem_paths(tls_root, &name)?;
        out.push(TlsIdentityConfig {
            server_name: name,
            cert: cert_path.to_string_lossy().into_owned(),
            key: key_path.to_string_lossy().into_owned(),
            // Substrate-declared identities default to "neither flag
            // set" — `compute_effective_flags` then promotes the first
            // row (typically a config row, since config rows come
            // first in the merge) to both. A substrate-only deployment
            // gets effective-default on whichever substrate row
            // sorts first; deterministic and matches the resolver's
            // own first-match rule.
            default: false,
            no_sni_fallback: false,
        });
    }
    // Stable order: lower-cased server_name. Two reload calls with the
    // same substrate state must produce byte-identical merge inputs so
    // the resolver-effective `(default, no_sni_fallback)` flags don't
    // shimmer.
    out.sort_by(|a, b| {
        a.server_name
            .to_ascii_lowercase()
            .cmp(&b.server_name.to_ascii_lowercase())
    });
    Ok(out)
}

/// Resolve `<tls_root>/<name>.{cert,key}.pem`, then verify the
/// canonical form of each path stays inside the canonical
/// `<tls_root>`. The substrate-write validator already rejects
/// `/`, `\\`, NUL, control bytes, `.`, and `..` in `<name>` — this
/// canonical-path check is defence-in-depth against a future
/// regression in the validator or an unforeseen interaction with
/// symlinks under `<tls_root>`.
///
/// PEMs need not exist at this point — that's checked separately
/// during the resolver build, where the failure surfaces as
/// "missing PEM" rather than "path traversal." We canonicalise
/// `<tls_root>` and the parent directory of the resolved paths
/// (the leaf PEM may not exist yet), then assert the canonical
/// parent equals the canonical `<tls_root>`.
///
/// If a leaf PEM exists at the resolved path, we additionally
/// canonicalise the leaf and assert its canonical parent is the
/// canonical `<tls_root>`. This catches the symlink-leaf escape
/// case where `<tls_root>/<name>.cert.pem` is a symlink pointing
/// outside the root — canonicalising the parent alone misses this
/// because the parent is `<tls_root>` itself.
fn canonical_pem_paths(tls_root: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    let cert_path = tls_root.join(format!("{name}.cert.pem"));
    let key_path = tls_root.join(format!("{name}.key.pem"));

    let root_canon = tls_root
        .canonicalize()
        .map_err(|e| anyhow!("canonicalise tls_root {:?}: {e}", tls_root.display()))?;
    // Both resolved paths share the same parent (`<tls_root>`).
    // Canonicalise the parent; the leaf PEM may not exist yet and
    // `Path::canonicalize` requires the full path to exist.
    let parent = cert_path
        .parent()
        .ok_or_else(|| anyhow!("resolved cert path has no parent: {}", cert_path.display()))?;
    let parent_canon = parent
        .canonicalize()
        .map_err(|e| anyhow!("canonicalise PEM parent {:?}: {e}", parent.display()))?;
    if parent_canon != root_canon {
        bail!(
            "PEM parent {:?} escapes tls_root {:?} (canonical {:?} vs {:?})",
            parent.display(),
            tls_root.display(),
            parent_canon.display(),
            root_canon.display()
        );
    }
    // If either leaf PEM exists, also canonicalise it and require
    // its canonical parent to equal the canonical `<tls_root>`. This
    // catches a symlink at `<tls_root>/<name>.cert.pem` (or `.key.pem`)
    // pointing outside the root: the parent check above passes
    // (`<tls_root>` is its own parent), but the leaf-canon check
    // below will resolve the symlink and reject if the target lives
    // elsewhere.
    for (label, leaf) in [("cert", &cert_path), ("key", &key_path)] {
        match leaf.canonicalize() {
            Ok(canon) => {
                let canon_parent = canon.parent().ok_or_else(|| {
                    anyhow!("canonical {label} PEM has no parent: {}", canon.display())
                })?;
                if canon_parent != root_canon {
                    bail!(
                        "{label} PEM {:?} canonicalises to {:?} which escapes tls_root {:?}",
                        leaf.display(),
                        canon.display(),
                        root_canon.display()
                    );
                }
            }
            // Leaf may legitimately be missing pre-deployment; the
            // resolver build surfaces this as "missing PEM" with
            // operator-friendly context.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow!(
                    "canonicalise {label} PEM {:?}: {e}",
                    leaf.display()
                ));
            }
        }
    }
    Ok((cert_path, key_path))
}

/// Merge config + substrate identities. Config rows come first (and
/// win on `server_name` collision via lowercased key); the returned
/// `collisions` list names every substrate row dropped this way so the
/// reload response can surface them as warnings.
fn merge_identities(
    baseline: &[TlsIdentityConfig],
    substrate: &[TlsIdentityConfig],
) -> (Vec<(TlsIdentityConfig, IdentitySource)>, Vec<String>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<(TlsIdentityConfig, IdentitySource)> = Vec::new();
    for cfg in baseline {
        let key = cfg.server_name.to_ascii_lowercase();
        if seen.insert(key) {
            merged.push((cfg.clone(), IdentitySource::Config));
        }
    }
    let mut collisions: Vec<String> = Vec::new();
    for cfg in substrate {
        let key = cfg.server_name.to_ascii_lowercase();
        if seen.insert(key.clone()) {
            merged.push((cfg.clone(), IdentitySource::Domain));
        } else {
            collisions.push(key);
        }
    }
    (merged, collisions)
}

async fn handle_reload(_cmd: &IncomingCommand, state: &TlsReloadState) -> Result<JsonValue> {
    let _guard = state.reload_lock.lock().await;

    // 1. Walk substrate identities, validate names + paths.
    let substrate = collect_substrate_identities(&state.domains_runtime, &state.tls_root).await?;
    let (merged, collisions) = merge_identities(&state.baseline, &substrate);

    // 2. Build the new resolver. The builder reads each PEM, parses,
    // and validates key↔cert match — a failure here keeps the prior
    // resolver in place (we have not yet stored anything).
    let configs: Vec<TlsIdentityConfig> = merged.iter().map(|(c, _)| c.clone()).collect();
    let new_resolver = if configs.is_empty() {
        None
    } else {
        Some(Arc::new(
            SniCertResolver::from_config(&configs, state.strict_sni)
                .map_err(|e| anyhow!("rebuild SniCertResolver: {e}"))?,
        ))
    };

    // 3. Re-project the read-only `maild.tls_identities` namespace.
    // Happens BEFORE the resolver swap so subscribers see identity
    // rows update before cert behaviour changes. A failure here also
    // keeps the prior resolver — the slot has not been touched yet.
    tls_identities::reproject_from_identities(
        &state.tls_identities_runtime,
        &merged,
        state.strict_sni,
    )
    .await?;

    // 4. Atomic swap, then clear the ServerConfig cache. The next
    // accept reads the new resolver Arc; in-flight handshakes drain
    // on the captured prior `Arc<ServerConfig>`.
    state.tls_slot.store(Arc::new(new_resolver));
    state.tls_config_cache.clear();

    let mut response = BTreeMap::new();
    response.insert(
        "identities".to_string(),
        JsonValue::Array(
            merged
                .iter()
                .map(|(c, src)| {
                    json!({
                        "server_name": c.server_name.to_ascii_lowercase(),
                        "source": match src {
                            IdentitySource::Config => "config",
                            IdentitySource::Domain => "domain",
                        },
                    })
                })
                .collect(),
        ),
    );
    if !collisions.is_empty() {
        response.insert(
            "warnings".to_string(),
            JsonValue::Array(
                collisions
                    .into_iter()
                    .map(|name| {
                        JsonValue::String(format!(
                            "substrate identity {name:?} collides with config row; config wins"
                        ))
                    })
                    .collect(),
            ),
        );
    }
    response.insert("strict_sni".to_string(), JsonValue::Bool(state.strict_sni));
    Ok(JsonValue::Object(response.into_iter().collect()))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
mod tests {
    use super::*;
    use cosmix_props::bus::mutation::PropsRouter;
    use cosmix_props::record::{Actor, RecordKey, Version};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::sqlite::SqliteStore;
    use rusqlite::Connection;
    use std::io::Write;
    use tempfile::TempDir;

    fn install_default_crypto_provider_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    fn write_pem_pair(dir: &Path, name: &str) {
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
    }

    fn cmd_reload() -> IncomingCommand {
        IncomingCommand {
            command: "maild.tls.reload".into(),
            from: String::new(),
            id: None,
            args: JsonValue::Null,
            headers: std::collections::BTreeMap::new(),
            body: String::new(),
        }
    }

    /// Build a fresh state with `baseline` config rows, plus a
    /// registered `maild.domains` runtime and a registered
    /// `maild.tls_identities` runtime sharing the same in-memory store.
    async fn fresh_state(
        tmp: &TempDir,
        baseline: Vec<TlsIdentityConfig>,
        strict_sni: bool,
    ) -> TlsReloadState {
        install_default_crypto_provider_once();
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        let tls_id_runtime = tls_identities::register(&mut router, &store, &baseline, strict_sni)
            .await
            .expect("register tls_identities");
        let domains_runtime = domains::register(&mut router, &store).expect("register domains");

        let initial_resolver = if baseline.is_empty() {
            None
        } else {
            Some(Arc::new(
                SniCertResolver::from_config(&baseline, strict_sni).expect("build initial"),
            ))
        };
        let slot = crate::tls::new_tls_slot(initial_resolver);
        let cache = Arc::new(ServerConfigCache::new());

        TlsReloadState::new(
            slot,
            cache,
            Arc::new(baseline),
            domains_runtime,
            tls_id_runtime,
            tmp.path().to_path_buf(),
            strict_sni,
        )
    }

    async fn set_domain_row_with_tls_identity(
        domains_runtime: &Arc<Runtime>,
        domain: &str,
        tls_identity_name: Option<&str>,
    ) {
        let mut body = match domains::defaults_value(domain) {
            PropValue::Object(m) => m,
            _ => unreachable!(),
        };
        match tls_identity_name {
            Some(name) => {
                body.insert("tls_identity_name".into(), PropValue::String(name.into()));
            }
            None => {
                body.insert("tls_identity_name".into(), PropValue::Null);
            }
        }
        let key = RecordKey::collection(domains::namespace_name(), domain.to_string());
        domains_runtime
            .set(
                key,
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: cosmix_props::store::MergeMode::Replace,
                    actor: Actor::operator("test"),
                    cause: Some("test seed".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed domains row");
    }

    // ---- Bucket A: reload picks up substrate-declared identity ----

    #[tokio::test]
    async fn reload_picks_up_substrate_identity_with_pems_at_canonical_path() {
        let tmp = TempDir::new().unwrap();
        write_pem_pair(tmp.path(), "example.net");
        let state = fresh_state(&tmp, Vec::new(), false).await;
        set_domain_row_with_tls_identity(
            &state.domains_runtime,
            "example.net",
            Some("example.net"),
        )
        .await;

        let (rc, body) = dispatch("reload", &cmd_reload(), &state).await;
        assert_eq!(rc, RC_OK, "body: {body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        let names: Vec<String> = v["identities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["server_name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["example.net"]);
        // Resolver slot populated.
        assert!(state.tls_slot.load_full().is_some());
        // tls_identities row materialised.
        let snap = state
            .tls_identities_runtime
            .store()
            .list(&tls_identities::namespace_name())
            .await
            .unwrap();
        assert_eq!(snap.value.len(), 1);
        assert_eq!(snap.value[0].key.key, "example.net");
        let row = match &snap.value[0].value {
            PropValue::Object(m) => m,
            _ => panic!("row not Object"),
        };
        assert_eq!(
            row.get("source"),
            Some(&PropValue::String("domain".into())),
            "substrate identity must surface as source=domain"
        );
    }

    // ---- Bucket C: substrate identity missing PEM keeps prior resolver ----

    #[tokio::test]
    async fn reload_missing_pem_keeps_prior_resolver() {
        let tmp = TempDir::new().unwrap();
        // Baseline config identity with on-disk PEMs.
        write_pem_pair(tmp.path(), "base.example");
        let baseline = vec![TlsIdentityConfig {
            server_name: "base.example".into(),
            cert: tmp
                .path()
                .join("base.example.cert.pem")
                .to_string_lossy()
                .into_owned(),
            key: tmp
                .path()
                .join("base.example.key.pem")
                .to_string_lossy()
                .into_owned(),
            default: true,
            no_sni_fallback: true,
        }];
        let state = fresh_state(&tmp, baseline, false).await;
        let prior = state.tls_slot.load_full();
        let prior_arc = prior.as_ref().as_ref().expect("prior present").clone();

        // Declare a substrate row pointing at PEMs that do NOT exist.
        set_domain_row_with_tls_identity(
            &state.domains_runtime,
            "ghost.example",
            Some("ghost.example"),
        )
        .await;

        let (rc, body) = dispatch("reload", &cmd_reload(), &state).await;
        assert_eq!(rc, RC_ERROR, "expected fail-loud, got body: {body}");

        // Prior resolver still served.
        let after = state.tls_slot.load_full();
        let after_arc = after.as_ref().as_ref().expect("still present").clone();
        assert!(
            Arc::ptr_eq(&prior_arc, &after_arc),
            "failed reload must keep the prior resolver Arc"
        );
    }

    // ---- Bucket C: config-vs-substrate name collision warns + config wins ----

    #[tokio::test]
    async fn reload_collision_warns_and_config_wins() {
        let tmp = TempDir::new().unwrap();
        write_pem_pair(tmp.path(), "dup.example");
        // Baseline: config identity at canonical path.
        let baseline = vec![TlsIdentityConfig {
            server_name: "dup.example".into(),
            cert: tmp
                .path()
                .join("dup.example.cert.pem")
                .to_string_lossy()
                .into_owned(),
            key: tmp
                .path()
                .join("dup.example.key.pem")
                .to_string_lossy()
                .into_owned(),
            default: true,
            no_sni_fallback: true,
        }];
        let state = fresh_state(&tmp, baseline, false).await;
        set_domain_row_with_tls_identity(
            &state.domains_runtime,
            "dup.example",
            Some("dup.example"),
        )
        .await;

        let (rc, body) = dispatch("reload", &cmd_reload(), &state).await;
        assert_eq!(rc, RC_OK, "body: {body}");
        let v: JsonValue = serde_json::from_str(&body).unwrap();
        let names: Vec<&str> = v["identities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["server_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["dup.example"]);
        let source = v["identities"][0]["source"].as_str().unwrap();
        assert_eq!(source, "config", "config row wins on collision");
        let warnings = v["warnings"].as_array().expect("warnings array");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].as_str().unwrap().contains("dup.example"),
            "warning names the colliding identity"
        );
    }

    // ---- Bucket C: tls_identity_name = "../escape" rejected at write ----

    #[tokio::test]
    async fn substrate_write_rejects_path_traversal_in_tls_identity_name() {
        // The hook fires on the substrate write — verifies the
        // validator wired up in props/domains.rs blocks the
        // `../escape` value before it can reach reload.
        let tmp = TempDir::new().unwrap();
        let state = fresh_state(&tmp, Vec::new(), false).await;
        let mut body = match domains::defaults_value("evil.example") {
            PropValue::Object(m) => m,
            _ => unreachable!(),
        };
        body.insert(
            "tls_identity_name".into(),
            PropValue::String("../escape".into()),
        );
        let key = RecordKey::collection(domains::namespace_name(), "evil.example".to_string());
        let res = state
            .domains_runtime
            .set(
                key,
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: cosmix_props::store::MergeMode::Replace,
                    actor: Actor::operator("test"),
                    cause: Some("path-traversal probe".into()),
                    ts_ms: 0,
                },
            )
            .await;
        assert!(res.is_err(), "validator must reject ../escape");
    }

    // ---- Bucket C: symlink leaf pointing outside tls_root is rejected ----

    #[tokio::test]
    #[cfg(unix)]
    async fn canonical_pem_paths_rejects_symlink_leaf_escape() {
        // The substrate-write validator already blocks `..` in the
        // identity name, so the parent of `<tls_root>/<name>.cert.pem`
        // is always `<tls_root>` itself by string construction. A
        // symlink planted at `<tls_root>/<name>.cert.pem` pointing
        // outside the root would defeat a parent-only canonicalisation
        // check — this test pins that the leaf-canonicalisation guard
        // catches it.
        let tls_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Write a real PEM elsewhere; symlink it into tls_root.
        std::fs::write(
            outside.path().join("foreign.cert.pem"),
            "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("foreign.cert.pem"),
            tls_root.path().join("victim.cert.pem"),
        )
        .unwrap();
        // key.pem need not exist for this guard to fire on the cert leaf.
        let err = canonical_pem_paths(tls_root.path(), "victim")
            .expect_err("symlink escape must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes tls_root") && msg.contains("canonicalises to"),
            "error must name the leaf-canonicalisation guard: {msg}"
        );
    }

    // ---- Bucket E: live-swap determinism (Arc::ptr_eq inequality) ----

    #[tokio::test]
    async fn each_successful_reload_produces_new_resolver_arc() {
        let tmp = TempDir::new().unwrap();
        write_pem_pair(tmp.path(), "swap.example");
        let baseline = vec![TlsIdentityConfig {
            server_name: "swap.example".into(),
            cert: tmp
                .path()
                .join("swap.example.cert.pem")
                .to_string_lossy()
                .into_owned(),
            key: tmp
                .path()
                .join("swap.example.key.pem")
                .to_string_lossy()
                .into_owned(),
            default: true,
            no_sni_fallback: true,
        }];
        let state = fresh_state(&tmp, baseline, false).await;
        let before = state.tls_slot.load_full();
        let before_arc = before.as_ref().as_ref().unwrap().clone();

        let (rc, body) = dispatch("reload", &cmd_reload(), &state).await;
        assert_eq!(rc, RC_OK, "body: {body}");
        let after = state.tls_slot.load_full();
        let after_arc = after.as_ref().as_ref().unwrap().clone();
        assert!(
            !Arc::ptr_eq(&before_arc, &after_arc),
            "successful reload must store a freshly-built resolver Arc"
        );
    }

    // ---- props.list reflects post-reload identity set ----

    #[tokio::test]
    async fn tls_identities_namespace_reflects_post_reload_set() {
        let tmp = TempDir::new().unwrap();
        write_pem_pair(tmp.path(), "one.example");
        write_pem_pair(tmp.path(), "two.example");
        let state = fresh_state(&tmp, Vec::new(), false).await;
        set_domain_row_with_tls_identity(
            &state.domains_runtime,
            "one.example",
            Some("one.example"),
        )
        .await;
        set_domain_row_with_tls_identity(
            &state.domains_runtime,
            "two.example",
            Some("two.example"),
        )
        .await;
        let (rc, body) = dispatch("reload", &cmd_reload(), &state).await;
        assert_eq!(rc, RC_OK, "body: {body}");
        let snap = state
            .tls_identities_runtime
            .store()
            .list(&tls_identities::namespace_name())
            .await
            .unwrap();
        let keys: HashSet<String> = snap.value.iter().map(|r| r.key.key.clone()).collect();
        assert!(keys.contains("one.example"));
        assert!(keys.contains("two.example"));
    }
}
