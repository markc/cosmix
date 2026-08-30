//! Reconstruct the per-domain DKIM `MailAuthSigner` from current state.
//!
//! Phase 4 introduces a second source of truth for DKIM signer rows
//! alongside the operator-edited `[[dkim.domain]]` config: the
//! `maild.domains` substrate namespace, whose `dkim_selectors` /
//! `dkim_selector_active` fields are driven by the Phase 4 verbs
//! (`maild.dkim.{generate,rotate,retire}`).
//!
//! `rebuild_signer_from_substrate` takes:
//!   * `baseline` — the immutable config-side configs already
//!     resolved by `Config::resolve_dkim_signer_configs` at startup.
//!   * `domains_runtime` — the props runtime that owns
//!     `maild.domains`. Used to read all rows and project them
//!     through `props::domains::view`.
//!   * `key_root` — directory under which substrate-managed PEMs
//!     live (convention: `<key_root>/<domain>/<selector>.pem`).
//!
//! Rule: a single domain cannot live in both sides. If a substrate
//! row has any `dkim_selectors`, and the same domain is also in
//! `baseline`, the rebuild is refused — picking which side wins
//! silently is a footgun, so it's surfaced. An active PEM whose
//! file is missing fails the rebuild; an inactive PEM whose file is
//! missing is logged as a warning and that selector is dropped (the
//! rotation transition tolerates a stale retired selector whose key
//! file has been cleaned away).

use anyhow::{Context, Result, bail};
use cosmix_maild_auth::{
    Canon, DkimSignerConfig, Domain, MailAuthSigner, default_signed_headers, detect_algorithm,
};
use cosmix_props::runtime::Runtime;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::props::domains::{namespace_name, view};

/// Convention path for a substrate-managed signer PEM. The verbs and
/// the rebuild loop both go through this so the on-disk layout stays
/// addressable from a single function.
pub fn pem_path(key_root: &Path, domain: &str, selector: &str) -> PathBuf {
    key_root.join(domain).join(format!("{selector}.pem"))
}

/// Read the PEM for a single substrate-managed signer row.
/// Returns `Ok(None)` if the file is absent and the caller should
/// treat that as a soft skip (inactive selectors); the caller is
/// responsible for promoting the missing-file case to a hard error
/// for active selectors.
pub fn read_pem_for(key_root: &Path, domain: &str, selector: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(pem_path(key_root, domain, selector))
}

/// Rebuild the in-memory signer set from `baseline + substrate`.
/// Returns `None` if both sides are empty (legacy single-key path
/// stays in charge) and `Some(signer)` otherwise.
pub async fn rebuild_signer_from_substrate(
    baseline: &[DkimSignerConfig],
    domains_runtime: &Arc<Runtime>,
    key_root: &Path,
) -> Result<Option<Arc<MailAuthSigner>>> {
    let ns = namespace_name();
    let snap = domains_runtime
        .store()
        .list(&ns)
        .await
        .context("list maild.domains substrate rows")?;

    let baseline_domains: HashSet<String> = baseline
        .iter()
        .map(|c| c.domain.as_str().to_string())
        .collect();

    let mut substrate_configs: Vec<DkimSignerConfig> = Vec::new();

    for record in &snap.value {
        // Unparseable rows are a hard error: silently skipping them
        // would let a corrupt row with overlapping `domain` slip past
        // the baseline-conflict check below. SPEC 12 §10 has reconcile
        // semantics for fixing on-disk corruption — startup is not the
        // place to paper over it.
        let v = view(record).with_context(|| {
            format!(
                "dkim_rebuild: unparseable maild.domains row {:?}",
                record.key.key
            )
        })?;
        if v.dkim_selectors.is_empty() {
            continue;
        }
        let domain_str = record.key.key.clone();
        if baseline_domains.contains(&domain_str) {
            bail!(
                "dkim_rebuild: domain {:?} appears in both [[dkim.domain]] config and \
                 maild.domains substrate (dkim_selectors=[{}]); pick one source of truth",
                domain_str,
                v.dkim_selectors.join(", ")
            );
        }
        let domain = Domain::new(domain_str.clone());

        for sel in &v.dkim_selectors {
            let is_active = v.dkim_selector_active.as_deref() == Some(sel.as_str());
            let path = pem_path(key_root, &domain_str, sel);
            let pem = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    if is_active {
                        bail!(
                            "dkim_rebuild: active selector {:?} for domain {:?} missing PEM at {}: {e}",
                            sel,
                            domain_str,
                            path.display()
                        );
                    } else {
                        tracing::warn!(
                            domain = %domain_str,
                            selector = %sel,
                            path = %path.display(),
                            error = %e,
                            "dkim_rebuild: inactive selector PEM missing — dropping selector"
                        );
                        continue;
                    }
                }
            };
            let algorithm = detect_algorithm(&pem).with_context(|| {
                format!("dkim_rebuild: detect algorithm for {domain_str}/{sel}")
            })?;
            MailAuthSigner::validate_pem(algorithm, &pem).map_err(|e| {
                anyhow::anyhow!("dkim_rebuild: validate key for {domain_str}/{sel}: {e}")
            })?;
            substrate_configs.push(DkimSignerConfig {
                domain: domain.clone(),
                selector: sel.clone(),
                algorithm,
                private_key_pem: pem,
                canonicalization: (Canon::Relaxed, Canon::Relaxed),
                headers_to_sign: default_signed_headers(),
                active_for_signing: is_active,
                allow_body_length_tag: false,
            });
        }
    }

    let mut merged: Vec<DkimSignerConfig> =
        Vec::with_capacity(baseline.len() + substrate_configs.len());
    merged.extend(baseline.iter().cloned());
    merged.extend(substrate_configs);

    if merged.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Arc::new(MailAuthSigner::new(merged))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::domains::{defaults_value, register as register_domains};
    use cosmix_maild_auth::{DkimAlgorithm, HeaderSpec, keygen};
    use cosmix_props::MergeMode;
    use cosmix_props::bus::mutation::PropsRouter;
    use cosmix_props::record::{Actor, Version};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::sqlite::SqliteStore;
    use cosmix_props::value::PropValue;
    use rusqlite::Connection;

    // ---- helpers ----

    fn rsa_baseline_cfg(domain: &str, selector: &str) -> DkimSignerConfig {
        let mat = keygen::generate(keygen::KeySpec::Rsa { bits: 2048 }).expect("keygen");
        DkimSignerConfig {
            domain: Domain::new(domain.to_string()),
            selector: selector.into(),
            algorithm: DkimAlgorithm::RsaSha256,
            private_key_pem: mat.private_pem.into_bytes(),
            canonicalization: (Canon::Relaxed, Canon::Relaxed),
            headers_to_sign: vec![HeaderSpec::Single("From".into())],
            active_for_signing: true,
            allow_body_length_tag: false,
        }
    }

    async fn fresh_runtime() -> Arc<Runtime> {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");
        register_domains(&mut router, &store).expect("register")
    }

    fn set_dkim_fields(body: &mut PropValue, selectors: Vec<&str>, active: Option<&str>) {
        if let PropValue::Object(map) = body {
            map.insert(
                "dkim_selectors".into(),
                PropValue::List(
                    selectors
                        .iter()
                        .map(|s| PropValue::String((*s).into()))
                        .collect(),
                ),
            );
            map.insert(
                "dkim_selector_active".into(),
                match active {
                    Some(s) => PropValue::String(s.into()),
                    None => PropValue::Null,
                },
            );
        }
    }

    async fn put_domain_row(
        runtime: &Arc<Runtime>,
        domain: &str,
        selectors: Vec<&str>,
        active: Option<&str>,
    ) {
        let mut body = defaults_value(domain);
        set_dkim_fields(&mut body, selectors, active);
        runtime
            .set(
                cosmix_props::record::RecordKey::collection(namespace_name(), domain.to_string()),
                body,
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("test"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect("set domain row");
    }

    fn write_pem(key_root: &Path, domain: &str, selector: &str) -> Vec<u8> {
        let mat = keygen::generate(keygen::KeySpec::Rsa { bits: 2048 }).expect("keygen");
        let path = pem_path(key_root, domain, selector);
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, mat.private_pem.as_bytes()).expect("write");
        mat.private_pem.into_bytes()
    }

    // ---- Bucket D: merge cases ----

    #[tokio::test]
    async fn merge_config_only_yields_baseline_signer() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        let baseline = vec![rsa_baseline_cfg("config.test", "c1")];
        let signer = rebuild_signer_from_substrate(&baseline, &runtime, key_root.path())
            .await
            .expect("rebuild");
        let signer = signer.expect("Some signer");
        signer
            .lookup_active(&Domain::new("config.test"))
            .expect("config.test active selector found");
    }

    #[tokio::test]
    async fn merge_substrate_only_yields_substrate_signer() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        write_pem(key_root.path(), "sub.test", "s1");
        put_domain_row(&runtime, "sub.test", vec!["s1"], Some("s1")).await;
        let signer = rebuild_signer_from_substrate(&[], &runtime, key_root.path())
            .await
            .expect("rebuild");
        let signer = signer.expect("Some signer");
        let cfg = signer
            .lookup_active(&Domain::new("sub.test"))
            .expect("sub.test active");
        assert_eq!(cfg.selector, "s1");
    }

    #[tokio::test]
    async fn merge_disjoint_domains_combines_both_sides() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        write_pem(key_root.path(), "sub.test", "s1");
        put_domain_row(&runtime, "sub.test", vec!["s1"], Some("s1")).await;
        let baseline = vec![rsa_baseline_cfg("config.test", "c1")];
        let signer = rebuild_signer_from_substrate(&baseline, &runtime, key_root.path())
            .await
            .expect("rebuild");
        let signer = signer.expect("Some signer");
        signer
            .lookup_active(&Domain::new("config.test"))
            .expect("config.test still resolvable");
        signer
            .lookup_active(&Domain::new("sub.test"))
            .expect("sub.test still resolvable");
    }

    #[tokio::test]
    async fn merge_same_domain_both_sides_errors() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        write_pem(key_root.path(), "dup.test", "s1");
        put_domain_row(&runtime, "dup.test", vec!["s1"], Some("s1")).await;
        let baseline = vec![rsa_baseline_cfg("dup.test", "c1")];
        let err = rebuild_signer_from_substrate(&baseline, &runtime, key_root.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("appears in both"),
            "expected baseline-conflict error, got: {err}"
        );
    }

    #[tokio::test]
    async fn merge_missing_active_pem_is_fatal() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        // Note: no PEM written.
        put_domain_row(&runtime, "miss.test", vec!["mk"], Some("mk")).await;
        let err = rebuild_signer_from_substrate(&[], &runtime, key_root.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("active selector") && err.contains("missing PEM"),
            "expected missing-active-PEM error, got: {err}"
        );
    }

    #[tokio::test]
    async fn merge_missing_inactive_pem_warns_and_skips() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        // Active key present, inactive key absent — rebuild must succeed.
        write_pem(key_root.path(), "warn.test", "active");
        put_domain_row(
            &runtime,
            "warn.test",
            vec!["active", "stale"],
            Some("active"),
        )
        .await;
        let signer = rebuild_signer_from_substrate(&[], &runtime, key_root.path())
            .await
            .expect("rebuild");
        let signer = signer.expect("Some signer");
        let cfg = signer
            .lookup_active(&Domain::new("warn.test"))
            .expect("active still resolvable");
        assert_eq!(cfg.selector, "active");
    }

    #[tokio::test]
    async fn merge_null_active_with_only_inactive_selector_skips_warn() {
        // dkim_selector_active=null + a selector listed → that selector
        // is inactive. PEM missing should warn-skip; the resulting
        // signer has no rows for this domain (and no other rows
        // anywhere), so rebuild returns None.
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        put_domain_row(&runtime, "noactive.test", vec!["s1"], None).await;
        let signer = rebuild_signer_from_substrate(&[], &runtime, key_root.path())
            .await
            .expect("rebuild");
        assert!(signer.is_none(), "no rows materialised → None");
    }

    #[tokio::test]
    async fn merge_empty_substrate_and_empty_baseline_yields_none() {
        let runtime = fresh_runtime().await;
        let key_root = tempfile::tempdir().expect("key_root");
        let signer = rebuild_signer_from_substrate(&[], &runtime, key_root.path())
            .await
            .expect("rebuild");
        assert!(signer.is_none());
    }
}
