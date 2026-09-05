//! SPEC 12 property-substrate adoption for `maild.tls_identities`.
//!
//! Read-only projection of the [`SniCertResolver`] state into the
//! substrate. One record per `[[tls.identity]]` config entry, keyed
//! by the (lower-cased) `server_name`. The substrate is *not* the
//! source of truth — maild reads cert files on startup, builds the
//! resolver, then materialises the resulting view here so operators
//! can introspect "which cert am I actually serving for which SNI"
//! over `maild.props.list namespace="tls_identities"`.
//!
//! ## Why read-only
//!
//! Phase 1 of the maild SNI / internal-CA plan does not mint or
//! rotate certs — that is Phase 3's job, behind the `pki.props.*`
//! verbs in `cosmix-pki`. This namespace's writes are denied at the
//! [`AuthPolicy`] layer (no `props.write` / `props.delete`
//! capabilities granted). The internal bootstrap call below uses
//! [`Runtime::set`] / [`Runtime::delete`] directly, which bypasses
//! the Bus authorisation layer; the substrate's auth check fires
//! only on Bus-routed `maild.props.set` / `maild.props.delete`
//! requests, so external callers cannot bypass the read-only intent.
//!
//! ## Cardinality + storage
//!
//! `Cardinality::Collection { primary_key_field: "server_name" }`
//! over the shared `__props_values` JSON table — no business-side
//! sidecar SQL needed.
//!
//! ## Reconciliation on every restart
//!
//! `register()` re-derives the namespace from the live resolver
//! input every startup: rows for `server_name`s no longer present
//! in `[[tls.identity]]` are deleted; rows for current identities
//! are written with current cert metadata. The substrate row IS the
//! current snapshot, not a write-ahead log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::{DeleteOpts, Runtime, SetOpts};
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;

use crate::config::TlsIdentityConfig;

/// Unqualified namespace name; fully qualified is
/// `maild.tls_identities`.
pub const NAMESPACE: &str = "tls_identities";

/// Primary-key field — matches the (lower-cased) wire `server_name`.
pub const PRIMARY_KEY_FIELD: &str = "server_name";

/// `before_set` / `before_delete` reject every operator-driven write
/// with this prefix so denials surface uniformly in client error
/// strings. Bootstrap writes happen with the internal
/// `Actor::service("maild")` and are accepted; everything else is a
/// misuse the AuthPolicy already blocks, but the hook is the
/// belt-and-braces guard.
pub const READONLY_PREFIX: &str = "tls_identities_readonly:";

/// Actor used by the bootstrap path. The hook recognises this exact
/// service name to allow internal writes through; anyone forging it
/// over the Bus wire would already have to satisfy the AuthPolicy
/// (which never grants write caps), so the hook check is a
/// defense-in-depth latch, not the primary gate.
pub const BOOTSTRAP_SERVICE: &str = "maild";

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// SPEC 12 §4.3 schema. Every field is required on Replace; Patch is
/// unreachable because writes are only ever issued by the internal
/// bootstrap, which always Replaces.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: PRIMARY_KEY_FIELD.into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Lower-cased SNI value this identity matches (RFC 6066)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "cert_path".into(),
            ty: FieldType::Path,
            default: None,
            secret: false,
            help: "On-disk path to the PEM cert chain".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "key_path".into(),
            ty: FieldType::Path,
            default: None,
            secret: false,
            help: "On-disk path to the PEM private key".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "default".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Whether this identity is the SNI-miss fallback and pre-TLS greeting host"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "no_sni_fallback".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Whether this identity is served when an implicit-TLS client sends no SNI"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "strict_sni".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Resolver-global flag — same on every row; mirrored here for read-side ergonomics"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "source".into(),
            ty: FieldType::String,
            default: Some(PropValue::String("config".into())),
            secret: false,
            help: "Provenance hint: \"config\" for TOML-supplied paths; \"cosmix-ca\" reserved for Phase 3"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "issuer".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "X.509 Issuer DN as parsed from the leaf cert".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "subject".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "X.509 Subject DN as parsed from the leaf cert".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "not_before".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "RFC 3339 timestamp — validity start of the leaf cert".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "not_after".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "RFC 3339 timestamp — validity end of the leaf cert".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "sans".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "DNS names from the Subject Alternative Name extension".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 §7.2 [`AuthPolicy`] — read/describe/audit caps granted to
/// every peer (WG `/24` trust domain). **No write/delete caps** — the
/// namespace is a projection of resolver state, not an operator
/// surface. Phase 3's `pki.props.*` is where mint/rotate lives.
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.tls_identities",
            "props.describe:maild.tls_identities:public",
            "props.describe:maild.tls_identities:full",
            "props.audit:maild.tls_identities",
        ]
        .into_iter()
        .map(|s| Capability::new(s).expect("non-empty capability"))
        .collect()
    }
    AuthPolicy::new(resolve)
}

/// Read-only hook handler. Bootstrap writes from the internal
/// `Actor::service("maild")` are accepted; every other actor is
/// rejected. `before_delete` follows the same rule so bootstrap can
/// reconcile away stale rows.
pub struct TlsIdentitiesHooks;

impl TlsIdentitiesHooks {
    fn require_bootstrap_actor(ctx: &HookCtx, op: &str) -> Result<(), HookError> {
        // `Actor` is a transparent newtype over the SPEC 07 §3.5.1
        // wire string — `Actor::service("maild")` renders to the bare
        // service name `maild`, distinct from `operator:<principal>`
        // / `daemon:<svc>` forms. Bootstrap is the only caller that
        // ever issues a bare service token over the Runtime API.
        if ctx.actor.as_str() == BOOTSTRAP_SERVICE {
            Ok(())
        } else {
            Err(HookError::validation(format!(
                "{READONLY_PREFIX} maild.tls_identities {op} is read-only — \
                 identity set is derived from [[tls.identity]] at startup"
            )))
        }
    }
}

impl HookHandler for TlsIdentitiesHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move { Self::require_bootstrap_actor(ctx, "set") })
    }

    fn before_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move { Self::require_bootstrap_actor(ctx, "delete") })
    }
}

pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: PRIMARY_KEY_FIELD.into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.lifecycle = NamespaceLifecycle::Simple;
    s.auth = auth_policy();
    s.hooks = hooks;
    s
}

/// Parsed cert metadata used to build the substrate row. Separated
/// from `register()` so the tests can drive it without going through
/// the filesystem.
#[derive(Debug, Clone)]
pub struct CertMetadata {
    pub issuer: String,
    pub subject: String,
    pub not_before: String,
    pub not_after: String,
    pub sans: Vec<String>,
}

/// Parse a PEM-encoded X.509 leaf certificate and extract the
/// substrate-visible metadata fields. Errors carry the offending path
/// (caller's responsibility) and the parser's own context.
pub fn parse_cert_metadata(cert_pem: &[u8]) -> Result<CertMetadata> {
    use x509_parser::extensions::ParsedExtension;
    use x509_parser::pem::Pem;
    use x509_parser::prelude::FromDer;

    let mut cursor = std::io::Cursor::new(cert_pem);
    let pem = Pem::read(&mut cursor)
        .context("reading PEM block from cert")?
        .0;
    if pem.label != "CERTIFICATE" {
        anyhow::bail!(
            "expected PEM label CERTIFICATE in leaf cert, got {:?}",
            pem.label
        );
    }
    let (_rem, cert) = x509_parser::certificate::X509Certificate::from_der(&pem.contents)
        .context("parsing DER X.509 leaf")?;

    let issuer = cert.tbs_certificate.issuer.to_string();
    let subject = cert.tbs_certificate.subject.to_string();
    let not_before = asn1time_to_rfc3339(&cert.tbs_certificate.validity.not_before);
    let not_after = asn1time_to_rfc3339(&cert.tbs_certificate.validity.not_after);

    let mut sans: Vec<String> = Vec::new();
    for ext in cert.tbs_certificate.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for general_name in &san.general_names {
                if let x509_parser::extensions::GeneralName::DNSName(dns) = general_name {
                    sans.push((*dns).to_string());
                }
            }
        }
    }

    Ok(CertMetadata {
        issuer,
        subject,
        not_before,
        not_after,
        sans,
    })
}

fn asn1time_to_rfc3339(t: &x509_parser::time::ASN1Time) -> String {
    // x509-parser exposes the ASN.1 instant as a unix timestamp via
    // `timestamp()`. Convert through chrono (already a workspace
    // dep) to avoid pulling `time` in as a direct dependency. RFC
    // 3339 is the substrate-side convention (matches engine_config
    // timestamps and JMAP-side ISO strings).
    let secs = t.timestamp();
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "INVALID".into())
}

/// Resolver-effective `(default, no_sni_fallback)` flags per
/// identity, mirroring `SniCertResolver::from_config` at
/// `crate::tls`. Two rules combine:
///
/// 1. **First-match-wins on duplicates.** The resolver picks
///    `default_idx` / `no_sni_idx` via `Iterator::position()`, so if
///    multiple rows carry the same role flag only the *first* one is
///    the resolver's effective fallback. Later flagged rows are
///    silently ignored for that role and we must report them as
///    effective-false to match what rustls actually serves.
/// 2. **Promote first row to both when no row carries either flag.**
///    Matches the resolver's "empty selection → row 0 takes both
///    roles" fallback.
pub fn compute_effective_flags(identities: &[TlsIdentityConfig]) -> Vec<(bool, bool)> {
    let any_default = identities.iter().any(|i| i.default);
    let any_no_sni = identities.iter().any(|i| i.no_sni_fallback);
    let (default_idx, no_sni_idx) = if !identities.is_empty() && !any_default && !any_no_sni {
        (Some(0usize), Some(0usize))
    } else {
        (
            identities.iter().position(|i| i.default),
            identities.iter().position(|i| i.no_sni_fallback),
        )
    };
    identities
        .iter()
        .enumerate()
        .map(|(i, _)| (default_idx == Some(i), no_sni_idx == Some(i)))
        .collect()
}

/// Provenance hint for a substrate row's `"source"` field. Phase 5a
/// commit-2 introduces `Domain` for identities declared via a
/// `maild.domains.tls_identity_name` substrate row; the existing
/// startup path keeps using `Config` for `[[tls.identity]]` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// Identity declared via the on-disk `[[tls.identity]]` config
    /// block. Surfaces as `"source": "config"` (matches the Phase 1
    /// substrate contract and the schema default at line 153).
    Config,
    /// Identity declared via a `maild.domains.tls_identity_name`
    /// substrate row pointing at canonical-path PEMs under
    /// `<tls_key_root>`. Surfaces as `"source": "domain"`.
    Domain,
}

impl IdentitySource {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Domain => "domain",
        }
    }
}

/// Build the substrate row body for a single identity. The
/// `effective_default` / `effective_no_sni_fallback` flags are the
/// *resolver-effective* values, not the raw [`TlsIdentityConfig`]
/// flags — see [`compute_effective_flags`] for the "first identity
/// becomes both" fallback rule that `SniCertResolver::from_config`
/// applies when no row carries either flag explicitly. Without that
/// mapping the substrate would lie about what cert is actually
/// served.
pub fn identity_row(
    cfg: &TlsIdentityConfig,
    metadata: &CertMetadata,
    strict_sni: bool,
    effective_default: bool,
    effective_no_sni_fallback: bool,
    source: IdentitySource,
) -> PropValue {
    let mut m = BTreeMap::new();
    let server_name = cfg.server_name.to_ascii_lowercase();
    m.insert(
        PRIMARY_KEY_FIELD.into(),
        PropValue::String(server_name.clone()),
    );
    m.insert("cert_path".into(), PropValue::String(cfg.cert.clone()));
    m.insert("key_path".into(), PropValue::String(cfg.key.clone()));
    m.insert("default".into(), PropValue::Bool(effective_default));
    m.insert(
        "no_sni_fallback".into(),
        PropValue::Bool(effective_no_sni_fallback),
    );
    m.insert("strict_sni".into(), PropValue::Bool(strict_sni));
    m.insert("source".into(), PropValue::String(source.as_str().into()));
    m.insert("issuer".into(), PropValue::String(metadata.issuer.clone()));
    m.insert(
        "subject".into(),
        PropValue::String(metadata.subject.clone()),
    );
    m.insert(
        "not_before".into(),
        PropValue::String(metadata.not_before.clone()),
    );
    m.insert(
        "not_after".into(),
        PropValue::String(metadata.not_after.clone()),
    );
    m.insert(
        "sans".into(),
        PropValue::List(
            metadata
                .sans
                .iter()
                .map(|s| PropValue::String(s.clone()))
                .collect(),
        ),
    );
    PropValue::Object(m)
}

/// Wire `maild.tls_identities` into the substrate and reconcile the
/// row set against `identities` + `strict_sni`. Idempotent across
/// restarts: rows for `server_name`s no longer in the config are
/// deleted; rows for current identities are Replaced with the
/// current cert metadata.
pub async fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    identities: &[TlsIdentityConfig],
    strict_sni: bool,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(TlsIdentitiesHooks);
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register tls_identities namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register tls_identities runtime on router: {e}"))?;

    // Startup-side wrapper: every config row is `IdentitySource::Config`.
    let tagged: Vec<(TlsIdentityConfig, IdentitySource)> = identities
        .iter()
        .cloned()
        .map(|c| (c, IdentitySource::Config))
        .collect();
    project_identities(&runtime, &tagged, strict_sni, "startup snapshot").await?;
    Ok(runtime)
}

/// Re-project the read-only `tls_identities` namespace from a freshly
/// merged identity list. Called by Phase 5a-commit-2's
/// `maild.tls.reload` verb BEFORE the resolver swap so subscribers see
/// identity rows update before cert behaviour changes.
///
/// `identities` carries explicit `(TlsIdentityConfig, IdentitySource)`
/// pairs so the namespace's `"source"` field surfaces correctly — the
/// resolver itself discards the provenance once it has built its
/// hostname → cert lookup.
pub async fn reproject_from_identities(
    runtime: &Arc<Runtime>,
    identities: &[(TlsIdentityConfig, IdentitySource)],
    strict_sni: bool,
) -> Result<()> {
    project_identities(runtime, identities, strict_sni, "reload reproject").await
}

/// Shared core for both [`register`] startup and
/// [`reproject_from_identities`] reload paths: reconcile the substrate
/// row set against the supplied (identity, source) list. Steps:
/// 1. Build the desired key set.
/// 2. Delete rows whose `server_name` is no longer present.
/// 3. Replace each currently-configured identity with freshly parsed
///    cert metadata, computing the resolver-effective `(default,
///    no_sni_fallback)` flags from the configs alone (the source tag
///    is metadata for the substrate row, not an input to the
///    resolver's first-match rule).
async fn project_identities(
    runtime: &Arc<Runtime>,
    identities: &[(TlsIdentityConfig, IdentitySource)],
    strict_sni: bool,
    cause_tail: &str,
) -> Result<()> {
    let mut desired_keys: BTreeSet<String> = BTreeSet::new();
    for (ident, _src) in identities {
        desired_keys.insert(ident.server_name.to_ascii_lowercase());
    }

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let actor = Actor::service(BOOTSTRAP_SERVICE).expect("valid bootstrap service actor");

    // Pre-pass: parse every PEM, read every prior row version, AND
    // list the existing namespace to compute the stale-row delete set
    // — all BEFORE mutating any substrate state. PEM-read / parse and
    // version-read failures are the most common runtime failure modes
    // (a cert deleted between collect and reproject, malformed PEM,
    // db transient); doing them up-front means an error aborts cleanly
    // with no half-applied namespace.
    //
    // Storage-layer mid-loop failures during the apply phase remain
    // possible, but the next reload's own pre-pass self-heals because
    // it re-walks `maild.domains` (the source of truth), not the
    // projection.
    let configs: Vec<TlsIdentityConfig> = identities.iter().map(|(c, _)| c.clone()).collect();
    let effective_flags = compute_effective_flags(&configs);
    let mut prepared: Vec<(RecordKey, PropValue, Version)> = Vec::with_capacity(identities.len());
    for (idx, (ident, source)) in identities.iter().enumerate() {
        let cert_bytes = std::fs::read(&ident.cert)
            .with_context(|| format!("reading cert {} for tls_identities row", ident.cert))?;
        let metadata = parse_cert_metadata(&cert_bytes)
            .with_context(|| format!("parsing cert {}", ident.cert))?;
        let (eff_default, eff_no_sni) = effective_flags[idx];
        let body = identity_row(
            ident,
            &metadata,
            strict_sni,
            eff_default,
            eff_no_sni,
            *source,
        );
        let key = RecordKey::collection(namespace_name(), ident.server_name.to_ascii_lowercase());
        // Tombstone-aware OCC anchor. `get()` hides tombstones (returns
        // NotFound → Version::zero), but the apply-phase `set` CAS below
        // compares `expected_version` against the REAL version, including a
        // tombstone's. Anchoring on `get()` would read zero for a
        // previously-removed identity, then crash-loop on
        // `version_mismatch: expected Version(0), current Version(N)` when
        // that identity is re-declared. `version_anchor` returns `Some(N)`
        // for a live OR tombstoned row and `None` only when truly absent, so
        // re-declaring a removed identity resurrects the row at its real
        // version instead of crashing.
        let expected_version = runtime
            .store()
            .version_anchor(&key)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "tls_identities reconcile: read prior version for {}: {e}",
                    ident.server_name
                )
            })?
            .unwrap_or_else(Version::zero);
        prepared.push((key, body, expected_version));
    }

    // Compute the stale-row delete set up front, so the apply phase
    // is purely "execute the planned mutations" with no I/O that
    // could surface a new failure mode.
    let snap = runtime
        .store()
        .list(&namespace_name())
        .await
        .map_err(|e| anyhow::anyhow!("tls_identities reconcile: list existing rows: {e}"))?;
    let stale_keys: Vec<String> = snap
        .value
        .into_iter()
        .filter_map(|record| {
            let key_str = record.key.key;
            if key_str.is_empty() || desired_keys.contains(&key_str) {
                None
            } else {
                Some(key_str)
            }
        })
        .collect();

    // 1. Delete stale rows. Failures are FATAL — a successful return
    // from this function means the projection matches `identities`, so
    // the caller (`maild.tls.reload`) can swap the resolver knowing the
    // substrate view is consistent. If a delete fails, bail; the
    // resolver swap does NOT happen and the prior resolver keeps
    // serving alongside the (partially-stale) projection. The next
    // reload re-converges.
    for key_str in &stale_keys {
        runtime
            .delete(
                RecordKey::collection(namespace_name(), key_str.clone()),
                DeleteOpts {
                    expected_version: None,
                    actor: actor.clone(),
                    cause: Some(format!(
                        "tls_identities reconcile ({cause_tail}): identity removed"
                    )),
                    ts_ms: now_ms,
                },
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("tls_identities reconcile: delete stale row {key_str}: {e}")
            })?;
        tracing::info!(server_name = %key_str, "tls_identities: stale row deleted");
    }

    // 2. Apply each pre-parsed row. Pre-pass guaranteed PEMs parsed
    // and version reads succeeded; any failure here is storage-layer
    // (db IO, expected_version CAS lost to a concurrent writer). The
    // next reload re-walks the source-of-truth namespace and converges.
    for (idx, (key, body, expected_version)) in prepared.into_iter().enumerate() {
        let server_name = &identities[idx].0.server_name;
        runtime
            .set(
                key,
                body,
                SetOpts {
                    expected_version: Some(expected_version),
                    merge: MergeMode::Replace,
                    actor: actor.clone(),
                    cause: Some(format!("tls_identities reconcile: {cause_tail}")),
                    ts_ms: now_ms,
                },
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("tls_identities reconcile: set row for {server_name}: {e}")
            })?;
    }

    tracing::info!(
        count = identities.len(),
        strict_sni,
        cause = %cause_tail,
        "tls_identities: rows reconciled"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, Version};
    use rusqlite::Connection;

    fn test_store() -> Arc<SqliteStore> {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        Arc::new(SqliteStore::new("maild", conn).expect("store"))
    }

    fn write_self_signed_pair(
        dir: &std::path::Path,
        stem: &str,
        sans: Vec<String>,
    ) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(sans).expect("rcgen");
        let cert_path = dir.join(format!("{stem}.crt"));
        let key_path = dir.join(format!("{stem}.key"));
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (
            cert_path.to_string_lossy().into_owned(),
            key_path.to_string_lossy().into_owned(),
        )
    }

    fn install_default_crypto_provider_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    #[test]
    fn schema_lists_all_fields() {
        let s = schema();
        let names: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "server_name",
                "cert_path",
                "key_path",
                "default",
                "no_sni_fallback",
                "strict_sni",
                "source",
                "issuer",
                "subject",
                "not_before",
                "not_after",
                "sans",
            ]
        );
    }

    #[test]
    fn cardinality_is_collection_keyed_by_server_name() {
        let s = spec(Hooks::new(TlsIdentitiesHooks));
        match &s.cardinality {
            Cardinality::Collection { primary_key_field } => {
                assert_eq!(primary_key_field, PRIMARY_KEY_FIELD);
            }
            other => panic!("expected Collection, got {other:?}"),
        }
    }

    #[test]
    fn auth_policy_grants_read_only_caps() {
        let policy = auth_policy();
        let caps = policy.resolve(&PeerIdentity::default());
        for c in [
            "props.read:maild.tls_identities",
            "props.describe:maild.tls_identities:public",
            "props.describe:maild.tls_identities:full",
            "props.audit:maild.tls_identities",
        ] {
            assert!(
                caps.contains(&Capability::new(c).expect("non-empty capability")),
                "grant {c}"
            );
        }
        // No write / delete caps.
        assert!(
            !caps.contains(
                &Capability::new("props.write:maild.tls_identities").expect("non-empty capability")
            ),
            "tls_identities is read-only"
        );
    }

    #[tokio::test]
    async fn before_set_rejects_non_bootstrap_actor() {
        let hooks = TlsIdentitiesHooks;
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "host.lan"),
            old: None,
            new: Some(PropValue::Object(BTreeMap::new())),
            version: Version::zero(),
            actor: Actor::operator("alice").expect("valid actor"),
            merge: Some(MergeMode::Replace),
            origin: cosmix_props::WriteOrigin::caller(),
        };
        let err = hooks.before_set(&ctx).await.unwrap_err();
        assert!(
            err.message().starts_with(READONLY_PREFIX),
            "got: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn before_set_accepts_bootstrap_actor() {
        let hooks = TlsIdentitiesHooks;
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "host.lan"),
            old: None,
            new: Some(PropValue::Object(BTreeMap::new())),
            version: Version::zero(),
            actor: Actor::service(BOOTSTRAP_SERVICE).expect("valid bootstrap service actor"),
            merge: Some(MergeMode::Replace),
            origin: cosmix_props::WriteOrigin::caller(),
        };
        hooks
            .before_set(&ctx)
            .await
            .expect("bootstrap actor allowed");
    }

    #[tokio::test]
    async fn before_delete_rejects_non_bootstrap_actor() {
        let hooks = TlsIdentitiesHooks;
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "host.lan"),
            old: Some(PropValue::Object(BTreeMap::new())),
            new: None,
            version: Version::zero(),
            actor: Actor::operator("alice").expect("valid actor"),
            merge: None,
            origin: cosmix_props::WriteOrigin::caller(),
        };
        let err = hooks.before_delete(&ctx).await.unwrap_err();
        assert!(err.message().starts_with(READONLY_PREFIX));
    }

    #[test]
    fn parse_cert_metadata_extracts_issuer_validity_sans() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, _key_path) = write_self_signed_pair(
            dir.path(),
            "host",
            vec!["host.example".into(), "alt.example".into()],
        );
        let cert_bytes = std::fs::read(&cert_path).unwrap();
        let md = parse_cert_metadata(&cert_bytes).expect("parse");
        // rcgen self-signs with CN=`rcgen self signed cert`; both
        // subject and issuer match that default. We assert
        // non-emptiness + DN-shape (a `CN=` token) rather than a
        // specific string — the parse path is what's under test.
        assert!(
            md.issuer.contains("CN="),
            "issuer should be an X.509 DN, got: {}",
            md.issuer
        );
        assert!(
            md.subject.contains("CN="),
            "subject should be an X.509 DN, got: {}",
            md.subject
        );
        // Validity strings are RFC 3339 (T...Z).
        assert!(
            md.not_before.contains("T") && md.not_before.ends_with("Z"),
            "not_before should be RFC 3339: {}",
            md.not_before
        );
        assert!(
            md.not_after.contains("T") && md.not_after.ends_with("Z"),
            "not_after should be RFC 3339: {}",
            md.not_after
        );
        // Both SANs round-trip.
        assert!(
            md.sans.contains(&"host.example".into()),
            "got: {:?}",
            md.sans
        );
        assert!(
            md.sans.contains(&"alt.example".into()),
            "got: {:?}",
            md.sans
        );
    }

    #[test]
    fn parse_cert_metadata_rejects_non_certificate_pem() {
        // A private-key PEM has label "PRIVATE KEY"; feeding it to
        // the cert parser must surface as an error rather than a
        // garbage row.
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (_cert_path, key_path) =
            write_self_signed_pair(dir.path(), "host", vec!["host.example".into()]);
        let key_bytes = std::fs::read(&key_path).unwrap();
        let err = parse_cert_metadata(&key_bytes).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("CERTIFICATE") || msg.contains("PEM"),
            "expected PEM-label error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn register_populates_one_row_per_identity() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a", vec!["a.example".into()]);
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b", vec!["b.example".into()]);

        let store = test_store();
        let mut router = PropsRouter::new("maild");
        let identities = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: cert_a,
                key: key_a,
                default: true,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: cert_b,
                key: key_b,
                default: false,
                no_sni_fallback: true,
            },
        ];

        let runtime = register(&mut router, &store, &identities, true)
            .await
            .expect("register");

        let snap = runtime.store().list(&namespace_name()).await.expect("list");
        assert_eq!(snap.value.len(), 2, "two rows expected");

        let mut by_key: BTreeMap<String, PropValue> = BTreeMap::new();
        for record in snap.value {
            by_key.insert(record.key.key.clone(), record.value);
        }

        let a = by_key.get("a.example").expect("a.example row");
        let obj = a.as_object().expect("object");
        assert_eq!(
            obj.get("default"),
            Some(&PropValue::Bool(true)),
            "a.example carries default=true"
        );
        assert_eq!(
            obj.get("strict_sni"),
            Some(&PropValue::Bool(true)),
            "strict_sni mirrored from resolver flag"
        );
        assert_eq!(
            obj.get("source"),
            Some(&PropValue::String("config".into())),
            "source defaults to \"config\" in Phase 1"
        );
        // Cert metadata fields populated.
        match obj.get("issuer") {
            Some(PropValue::String(s)) => assert!(!s.is_empty(), "issuer non-empty"),
            other => panic!("issuer missing or wrong type: {other:?}"),
        }
        match obj.get("sans") {
            Some(PropValue::List(l)) => {
                assert!(
                    l.iter()
                        .any(|v| matches!(v, PropValue::String(s) if s == "a.example")),
                    "SAN list must contain a.example: {l:?}"
                );
            }
            other => panic!("sans missing or wrong type: {other:?}"),
        }

        let b = by_key.get("b.example").expect("b.example row");
        let obj = b.as_object().expect("object");
        assert_eq!(
            obj.get("no_sni_fallback"),
            Some(&PropValue::Bool(true)),
            "b.example carries no_sni_fallback=true"
        );
    }

    #[tokio::test]
    async fn register_reconciles_stale_rows_on_restart() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a", vec!["a.example".into()]);
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b", vec!["b.example".into()]);
        let (cert_c, key_c) = write_self_signed_pair(dir.path(), "c", vec!["c.example".into()]);

        let store = test_store();

        // First boot: two identities (a + b).
        {
            let mut router = PropsRouter::new("maild");
            let identities = vec![
                TlsIdentityConfig {
                    server_name: "a.example".into(),
                    cert: cert_a.clone(),
                    key: key_a.clone(),
                    default: true,
                    no_sni_fallback: true,
                },
                TlsIdentityConfig {
                    server_name: "b.example".into(),
                    cert: cert_b.clone(),
                    key: key_b.clone(),
                    default: false,
                    no_sni_fallback: false,
                },
            ];
            let _runtime = register(&mut router, &store, &identities, false)
                .await
                .expect("first register");
        }

        // Second boot: drop b, add c. Expect a + c, no b.
        {
            let mut router = PropsRouter::new("maild");
            let identities = vec![
                TlsIdentityConfig {
                    server_name: "a.example".into(),
                    cert: cert_a,
                    key: key_a,
                    default: true,
                    no_sni_fallback: true,
                },
                TlsIdentityConfig {
                    server_name: "c.example".into(),
                    cert: cert_c,
                    key: key_c,
                    default: false,
                    no_sni_fallback: false,
                },
            ];
            let runtime = register(&mut router, &store, &identities, false)
                .await
                .expect("second register");
            let snap = runtime.store().list(&namespace_name()).await.expect("list");
            let keys: BTreeSet<String> = snap.value.iter().map(|r| r.key.key.clone()).collect();
            assert!(keys.contains("a.example"), "a.example survives");
            assert!(keys.contains("c.example"), "c.example added");
            assert!(!keys.contains("b.example"), "b.example reconciled away");
        }
    }

    #[tokio::test]
    async fn register_resurrects_tombstoned_identity_on_redeclare() {
        // Regression for the crash-loop: an identity removed on one
        // boot (tombstoned at version N) and re-declared on a later boot
        // must reconcile to N+1, not crash with
        // `version_mismatch: expected Version(0), current Version(N)`.
        // `get()` hides the tombstone (NotFound → zero) while the apply
        // CAS sees N; `version_anchor` closes that gap.
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a", vec!["a.example".into()]);

        let store = test_store();

        let ident_a = || TlsIdentityConfig {
            server_name: "a.example".into(),
            cert: cert_a.clone(),
            key: key_a.clone(),
            default: true,
            no_sni_fallback: false,
        };

        // Boot 1: declare a.example.
        {
            let mut router = PropsRouter::new("maild");
            register(&mut router, &store, &[ident_a()], false)
                .await
                .expect("first register declares a.example");
        }

        // Boot 2: drop a.example entirely → its row is tombstoned.
        {
            let mut router = PropsRouter::new("maild");
            let runtime = register(&mut router, &store, &[], false)
                .await
                .expect("second register removes a.example");
            let snap = runtime.store().list(&namespace_name()).await.expect("list");
            assert!(
                snap.value.iter().all(|r| r.key.key != "a.example"),
                "a.example tombstoned away"
            );
        }

        // Boot 3: re-declare a.example. Pre-fix this crash-looped on the
        // tombstone version; it must now resurrect the row.
        {
            let mut router = PropsRouter::new("maild");
            let runtime = register(&mut router, &store, &[ident_a()], false)
                .await
                .expect("third register resurrects a.example (not version_mismatch)");
            let snap = runtime.store().list(&namespace_name()).await.expect("list");
            assert!(
                snap.value.iter().any(|r| r.key.key == "a.example"),
                "a.example resurrected after re-declare"
            );
        }
    }

    #[test]
    fn compute_effective_flags_promotes_first_when_no_row_carries_either() {
        // Mirrors `SniCertResolver::from_config`: when neither
        // `default` nor `no_sni_fallback` is set on any identity, the
        // resolver treats the first row as both. The substrate row
        // must report the same effective flags or it would lie about
        // what cert rustls actually serves.
        let identities = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: false,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: false,
                no_sni_fallback: false,
            },
        ];
        let eff = compute_effective_flags(&identities);
        assert_eq!(eff, vec![(true, true), (false, false)]);
    }

    #[test]
    fn compute_effective_flags_duplicate_flag_first_match_wins() {
        // The resolver picks `default_idx` / `no_sni_idx` via
        // `position()`. If two rows carry `default=true`, only the
        // first is the effective default; the second is dead weight
        // at the resolver layer and must read as effective-false in
        // the substrate row.
        let identities = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: true,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: true,
                no_sni_fallback: true,
            },
            TlsIdentityConfig {
                server_name: "c.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: false,
                no_sni_fallback: true,
            },
        ];
        let eff = compute_effective_flags(&identities);
        assert_eq!(
            eff,
            vec![(true, false), (false, true), (false, false)],
            "first default=true wins; first no_sni_fallback=true wins"
        );
    }

    #[test]
    fn compute_effective_flags_preserves_explicit_flags() {
        // Any row carrying either flag suppresses the "promote first"
        // fallback — the resolver respects explicit operator intent
        // and so must the substrate row.
        let identities = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: false,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: "/dev/null".into(),
                key: "/dev/null".into(),
                default: true,
                no_sni_fallback: false,
            },
        ];
        let eff = compute_effective_flags(&identities);
        assert_eq!(eff, vec![(false, false), (true, false)]);
    }

    #[tokio::test]
    async fn register_materialises_effective_flags_when_config_omits_both() {
        // End-to-end pin: with neither `default` nor `no_sni_fallback`
        // set in config, the first substrate row must carry both
        // flags effective-true and the second must carry both false.
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a", vec!["a.example".into()]);
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b", vec!["b.example".into()]);

        let store = test_store();
        let mut router = PropsRouter::new("maild");
        let identities = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: cert_a,
                key: key_a,
                default: false,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: cert_b,
                key: key_b,
                default: false,
                no_sni_fallback: false,
            },
        ];

        let runtime = register(&mut router, &store, &identities, false)
            .await
            .expect("register");
        let snap = runtime.store().list(&namespace_name()).await.expect("list");

        let mut by_key: BTreeMap<String, PropValue> = BTreeMap::new();
        for record in snap.value {
            by_key.insert(record.key.key.clone(), record.value);
        }

        let a = by_key
            .get("a.example")
            .expect("a.example row")
            .as_object()
            .unwrap();
        assert_eq!(
            a.get("default"),
            Some(&PropValue::Bool(true)),
            "first row gets effective default=true when no row carries flags"
        );
        assert_eq!(
            a.get("no_sni_fallback"),
            Some(&PropValue::Bool(true)),
            "first row gets effective no_sni_fallback=true when no row carries flags"
        );

        let b = by_key
            .get("b.example")
            .expect("b.example row")
            .as_object()
            .unwrap();
        assert_eq!(b.get("default"), Some(&PropValue::Bool(false)));
        assert_eq!(b.get("no_sni_fallback"), Some(&PropValue::Bool(false)));
    }

    #[tokio::test]
    async fn register_lowercases_server_name_for_key() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_self_signed_pair(dir.path(), "h", vec!["host.example".into()]);

        let store = test_store();
        let mut router = PropsRouter::new("maild");
        let identities = vec![TlsIdentityConfig {
            server_name: "Host.Example".into(),
            cert,
            key,
            default: true,
            no_sni_fallback: true,
        }];

        let runtime = register(&mut router, &store, &identities, false)
            .await
            .expect("register");
        let snap = runtime.store().list(&namespace_name()).await.expect("list");
        assert_eq!(snap.value.len(), 1);
        assert_eq!(
            snap.value[0].key.key.as_str(),
            "host.example",
            "primary key lower-cased like resolver does"
        );
    }
}
