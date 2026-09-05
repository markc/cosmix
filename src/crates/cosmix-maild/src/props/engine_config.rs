//! SPEC 12 property-substrate adoption for `maild.engine_config`.
//!
//! Singleton-cardinality namespace carrying the rules engine's
//! twelve globals: nine v0.1.0 fields (threshold, hard_junk_threshold,
//! shadow_mode, body_scan_bytes, url_extraction_cap, header_line_cap,
//! header_line_byte_cap, budget_ms, mail_auth_hard_fail_kinds) plus
//! three v0.2.0 DNSBL fields (dnsbl_query_timeout_ms,
//! dnsbl_negative_ttl_secs, dnsbl_resolver_addresses) added by the
//! maild-rules Phase 2 amendment. Phase 3 C1 shipped the namespace,
//! schema, and validation hooks; C3 wires `after_set` into the running
//! `DefaultRuleEngine` via an `Arc<OnceLock<Arc<DefaultRuleEngine>>>`
//! populated by `main`; C4 added bootstrap-from-substrate.
//!
//! ## Schema evolution (v0.2.0 DNSBL amendment)
//!
//! The amendment follows SPEC 12 §12: every new field carries
//! `since: Some(SchemaVersion::new(0, 2, 0))` and a default that
//! mirrors `EngineConfig::default()`. Existing v0.1.0 rows on disk
//! stay literally 9-field; only a Replace carrying the full 12-field
//! body densifies stored bytes (strict Replace requires every field),
//! and a Patch only adds whichever amendment fields the patch
//! explicitly includes. The substrate commits the raw client value —
//! `before_set` returns `Result<(), Error>`, not a transformed value —
//! so [`fill_amendment_defaults`] inside [`Self::effective_value`]
//! affects what the validator sees, not what gets stored. The read
//! path ([`record_to_engine_config`]) supplies the defaults at engine
//! read time so the engine always sees a full 12-field config
//! regardless of the on-disk shape. `<svc>.props.get` returns the
//! literal on-disk shape (SPEC 12 §10 audit-digest reproducibility
//! requires this — see `cosmix-lib-props-store/src/bus/mutation.rs::project_fields`).
//! Validation fills amendment defaults before per-field checks so a
//! Patch from a legacy operator script that touches only v0.1.0
//! fields does not regress on DNSBL bounds checks (the absent
//! amendment fields are validated against their defaults, which
//! are by construction in range).
//!
//! ## Lifecycle
//!
//! `Simple` — no provisioning side-effects. `before_set` validates the
//! caller body (computing the merged effective value for Patch over an
//! existing row) and `before_delete` refuses every delete (the engine
//! depends on this row existing post-bootstrap). The substrate commits
//! the value into `__props_values` via the bundled
//! [`JsonValuesMapping`].
//!
//! ## Cardinality
//!
//! `Singleton { canonical_key: "current" }`. Wire `key:` on requests is
//! the empty string; substrate responses fill `key: "current"`.
//!
//! ## Source of truth
//!
//! After Phase 3 lands C4, the substrate row IS the engine config. The
//! TOML `Config` struct is intentionally NOT extended with these
//! fields; adding a TOML layer would create a dual-source problem we
//! already decided against for `account_overrides`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use cosmix_maild_rules::{DefaultRuleEngine, EngineConfig, MailAuthHardFailKind};
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, SchemaVersion, StorageBackendKind,
};
use cosmix_props::record::RecordKey;
use cosmix_props::runtime::Runtime;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::store::{MergeMode, PropertyStore, StoreError};
use cosmix_props::value::PropValue;
use tokio::sync::Mutex;

/// Unqualified namespace name; fully qualified is `maild.engine_config`.
pub const NAMESPACE: &str = "engine_config";

/// Wire key the substrate fills in on responses. Set requests carry the
/// empty string per `Cardinality::Singleton` semantics.
pub const CANONICAL_KEY: &str = "current";

/// `before_set` rejects `MergeMode::Replace` requests that omit any
/// required field (the schema enforces shape, but Replace semantics
/// require the full body so a half-written replacement cannot land).
pub const REPLACE_REQUIRES_FULL_BODY_PREFIX: &str = "engine_config_replace_requires_full_body:";

/// `before_set` rejects `MergeMode::Patch` against an absent row. The
/// substrate's Patch semantics merge with the prior; without one,
/// silently materialising `defaults + patch` would invent a behaviour
/// the substrate does not carry. Operators wanting first-time init use
/// Replace.
pub const PATCH_REQUIRES_EXISTING_ROW_PREFIX: &str = "engine_config_patch_requires_existing_row:";

/// `before_delete` rejects every delete — the engine depends on this
/// singleton existing once bootstrap completes. Operators wanting
/// "back to defaults" issue a Replace with the defaults body.
pub const UNDELETABLE_PREFIX: &str = "engine_config_undeletable:";

/// Schema version carrying the DNSBL field amendment (maild-rules
/// Phase 2). Per-field `since:` markers on `dnsbl_query_timeout_ms`,
/// `dnsbl_negative_ttl_secs`, and `dnsbl_resolver_addresses` cite this
/// version so a substrate consumer reading the schema can tell which
/// fields are guaranteed-present in legacy rows (v0.1.0) and which
/// must be read as defaults when absent.
pub const DNSBL_AMENDMENT_VERSION: SchemaVersion = SchemaVersion::new(0, 2, 0);

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// The list of `MailAuthHardFailKind` discriminants as strings. Mirrors
/// the Rust enum at `cosmix_maild_rules::types::MailAuthHardFailKind`.
/// Adding a new variant in Rust requires extending this list (and the
/// converter below); the hook is the only enforcer that a list entry
/// is a known variant — `FieldType::Enum` is descriptive at the
/// substrate level today.
pub fn mail_auth_hard_fail_kind_variants() -> Vec<String> {
    vec![
        "SpfFailDmarcReject".into(),
        "DkimFailDmarcReject".into(),
        "BothAuthFailNoArc".into(),
    ]
}

fn parse_mail_auth_kind(s: &str) -> Option<MailAuthHardFailKind> {
    match s {
        "SpfFailDmarcReject" => Some(MailAuthHardFailKind::SpfFailDmarcReject),
        "DkimFailDmarcReject" => Some(MailAuthHardFailKind::DkimFailDmarcReject),
        "BothAuthFailNoArc" => Some(MailAuthHardFailKind::BothAuthFailNoArc),
        _ => None,
    }
}

#[allow(dead_code)] // consumed by engine_config_to_value (test-only in C1; live in C3+).
fn mail_auth_kind_to_string(k: &MailAuthHardFailKind) -> &'static str {
    match k {
        MailAuthHardFailKind::SpfFailDmarcReject => "SpfFailDmarcReject",
        MailAuthHardFailKind::DkimFailDmarcReject => "DkimFailDmarcReject",
        MailAuthHardFailKind::BothAuthFailNoArc => "BothAuthFailNoArc",
    }
}

/// Build a substrate-shape `PropValue::Object` carrying the defaults
/// row. The C4 bootstrap path writes this when the substrate row is
/// absent on first boot.
#[allow(dead_code)] // consumed by C4 bootstrap path; tests exercise it now.
pub fn defaults_value() -> PropValue {
    engine_config_to_value(&EngineConfig::default())
}

/// Serialise an `EngineConfig` into the substrate-shape value the
/// schema expects. Used by C4 bootstrap and any future operator-facing
/// "read defaults" path.
#[allow(dead_code)] // consumed by defaults_value + C4 bootstrap; tests exercise it now.
pub fn engine_config_to_value(cfg: &EngineConfig) -> PropValue {
    let mut m = BTreeMap::new();
    m.insert(
        "threshold".into(),
        PropValue::Float(f64::from(cfg.threshold)),
    );
    m.insert(
        "hard_junk_threshold".into(),
        PropValue::Float(f64::from(cfg.hard_junk_threshold)),
    );
    m.insert("shadow_mode".into(), PropValue::Bool(cfg.shadow_mode));
    m.insert(
        "body_scan_bytes".into(),
        PropValue::UInt(cfg.body_scan_bytes as u64),
    );
    m.insert(
        "url_extraction_cap".into(),
        PropValue::UInt(u64::from(cfg.url_extraction_cap)),
    );
    m.insert(
        "header_line_cap".into(),
        PropValue::UInt(u64::from(cfg.header_line_cap)),
    );
    m.insert(
        "header_line_byte_cap".into(),
        PropValue::UInt(cfg.header_line_byte_cap as u64),
    );
    m.insert(
        "budget_ms".into(),
        PropValue::UInt(u64::from(cfg.budget_ms)),
    );
    let kinds: Vec<PropValue> = cfg
        .mail_auth_hard_fail_kinds
        .iter()
        .map(|k| PropValue::String(mail_auth_kind_to_string(k).into()))
        .collect();
    m.insert("mail_auth_hard_fail_kinds".into(), PropValue::List(kinds));
    // v0.2.0 DNSBL amendment fields.
    m.insert(
        "dnsbl_query_timeout_ms".into(),
        PropValue::UInt(u64::from(cfg.dnsbl_query_timeout_ms)),
    );
    m.insert(
        "dnsbl_negative_ttl_secs".into(),
        PropValue::UInt(u64::from(cfg.dnsbl_negative_ttl_secs)),
    );
    let addrs: Vec<PropValue> = cfg
        .dnsbl_resolver_addresses
        .iter()
        .map(|a| PropValue::String(a.to_string()))
        .collect();
    m.insert("dnsbl_resolver_addresses".into(), PropValue::List(addrs));
    PropValue::Object(m)
}

/// SPEC 12 §4.3 schema. Defaults mirror `EngineConfig::default()` so a
/// Patch over the bootstrap row that omits a field produces the same
/// effective value the engine boots with.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "threshold".into(),
            ty: FieldType::F64,
            default: Some(PropValue::Float(5.0)),
            secret: false,
            help: "Continue-vs-HardJunk soft threshold (raw rule score; engine reads as f32)"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "hard_junk_threshold".into(),
            ty: FieldType::F64,
            default: Some(PropValue::Float(15.0)),
            secret: false,
            help:
                "raw_score above this is HardJunk{ScoreBreach}; must be >= threshold (engine reads as f32)"
                    .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "shadow_mode".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "When true, HardJunk verdicts are downgraded to Continue with would_junk=true"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "body_scan_bytes".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(1_048_576)),
            secret: false,
            help: "Body bytes scanned per classification (1..=67_108_864; ceiling is 64 MiB)"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "url_extraction_cap".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(1024)),
            secret: false,
            help: "Maximum URLs extracted per message (1..=1_000_000; engine reads as u32)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "header_line_cap".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(256)),
            secret: false,
            help: "Maximum header lines considered (1..=100_000; engine reads as u32)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "header_line_byte_cap".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(8192)),
            secret: false,
            help: "Maximum bytes per header line (1..=1_048_576)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "budget_ms".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(50)),
            secret: false,
            help: "Wall-clock budget per classify() call in ms (1..=600_000; engine reads as u32)"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "mail_auth_hard_fail_kinds".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::Enum {
                    variants: mail_auth_hard_fail_kind_variants(),
                }),
            },
            default: Some(PropValue::List(vec![
                PropValue::String("SpfFailDmarcReject".into()),
                PropValue::String("DkimFailDmarcReject".into()),
            ])),
            secret: false,
            help: "Mail-auth combinations that produce HardJunk regardless of other rules".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        // v0.2.0 DNSBL amendment — maild-rules Phase 2. Each carries a
        // `since` marker and a default mirroring `EngineConfig::default()`
        // so pre-amendment rows on disk read as the engine-domain defaults
        // (SPEC 12 §12) until an operator Replace upgrades them.
        FieldSchema {
            name: "dnsbl_query_timeout_ms".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(2000)),
            secret: false,
            help: "Per-DNSBL-query wall-clock budget in ms (1..=60_000; engine reads as u32)"
                .into(),
            since: Some(DNSBL_AMENDMENT_VERSION),
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dnsbl_negative_ttl_secs".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(300)),
            secret: false,
            help: "TTL (s) for cached negatives — NXDomain / NoError-no-A (1..=86_400; engine reads as u32)"
                .into(),
            since: Some(DNSBL_AMENDMENT_VERSION),
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dnsbl_resolver_addresses".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Override DNSBL resolvers as 'ip:port' socket pairs. Empty = use system resolver."
                .into(),
            since: Some(DNSBL_AMENDMENT_VERSION),
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 §7.2 [`AuthPolicy`] — phase-3 grants every property
/// capability to every reachable peer (WireGuard `/24` trust domain,
/// same posture as Phase 1/2). Phase N+ narrows.
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.engine_config",
            "props.write:maild.engine_config",
            "props.describe:maild.engine_config:public",
            "props.describe:maild.engine_config:full",
            "props.audit:maild.engine_config",
        ]
        .into_iter()
        .map(|s| Capability::new(s).expect("non-empty capability"))
        .collect()
    }
    AuthPolicy::new(resolve)
}

pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Singleton {
            canonical_key: CANONICAL_KEY.into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    // SPEC 12 §12 — the per-namespace schema version. `describe` projects
    // this into `schema_version` so clients can decide which amendment
    // a stored row belongs to. The v0.2.0 DNSBL amendment is the first
    // schema bump for this namespace; pre-amendment rows on disk still
    // belong to v0.1.0 (no `since:` markers on the original nine fields)
    // but the namespace as a whole advertises v0.2.0 once amended.
    s.since_version = DNSBL_AMENDMENT_VERSION;
    s.lifecycle = NamespaceLifecycle::Simple;
    // SPEC 12 §4.4 — `require_version: true` closes the hook/commit
    // TOCTOU window for cross-field invariants like `hard_junk_threshold
    // >= threshold`. `before_set` validates against a snapshot of the
    // prior row taken before `commit_set` re-merges in-tx; without CAS,
    // a concurrent Replace from another writer could land between the
    // hook and the commit, leaving the substrate row in a state the
    // hook would have rejected. With `require_version=true`, every
    // `props.set` carries `if_version`, and a racing writer surfaces as
    // an explicit version conflict on the loser's commit rather than a
    // silent invariant violation. The C4 bootstrap writes the default
    // row with `expected_version = Version::zero()` for create-if-absent.
    // `delete_mode` stays at the default `SoftDelete`; `HardDelete` is
    // incompatible with `require_version=true` (§5.4) and deletes are
    // unconditionally rejected by `before_delete` anyway.
    s.require_version = true;
    s.auth = auth_policy();
    s.hooks = hooks;
    s
}

/// Hook handler for the `engine_config` namespace.
///
/// **C1:** `before_set` performs full effective-value validation
/// branching on `ctx.merge`; `before_delete` rejects every delete.
///
/// **C3 (this commit):** `after_set` reads the just-committed row back
/// from the substrate, re-validates via [`record_to_engine_config`]
/// (defensive — guards against a corrupted row that snuck past the
/// hook), and atomically swaps the live engine's config via
/// [`DefaultRuleEngine::set_config`].
///
/// Concurrency: hooks are serialized through `apply_lock` so two
/// parallel writers' `after_set` invocations cannot interleave their
/// read-then-apply sequences. Combined with the substrate's
/// `require_version=true` commit serialization (SPEC 12 §4.4), each
/// `after_set` reads the *current* row at lock-acquisition time;
/// whichever hook holds the lock last leaves the engine matching the
/// latest committed row.
///
/// Lifecycle: Simple — an `Err` returned here becomes a `warning` on
/// the props.set response per SPEC 12 §6.5 (the row is durable
/// regardless). That means a transient engine-side failure during
/// after_set does NOT roll back the substrate write, and the next
/// successful write — or a deliberate Replace of the current row —
/// will re-attempt the engine swap. C3 only ever returns `Err` for a
/// substrate read or row-validation failure; the engine swap itself
/// is infallible.
pub struct EngineConfigHooks {
    /// Engine handle, populated by `main` after the engine is
    /// constructed. Empty during the registration → construction
    /// window (and in tests that do not wire an engine); `after_set`
    /// returns `Ok(())` as a no-op in that case so the substrate row
    /// still lands cleanly.
    engine: Arc<OnceLock<Arc<DefaultRuleEngine>>>,
    /// Serializes the read-then-apply sequence inside `after_set` so
    /// two parallel writers cannot leave the engine state observing
    /// a stale row after a later commit has already landed.
    apply_lock: Arc<Mutex<()>>,
    /// Backing store, used by `after_set` to re-read the row at
    /// lock-acquisition time (rather than trusting `ctx.new`, which
    /// snapshots the value at hook-fire time and may already be
    /// behind a later concurrent commit).
    store: Arc<SqliteStore>,
}

impl EngineConfigHooks {
    pub fn new(engine: Arc<OnceLock<Arc<DefaultRuleEngine>>>, store: Arc<SqliteStore>) -> Self {
        Self {
            engine,
            apply_lock: Arc::new(Mutex::new(())),
            store,
        }
    }

    /// Compute the effective value the substrate will commit, branching
    /// on merge mode. Returns the value to validate.
    ///
    /// - `Some(MergeMode::Replace)`: the caller body is the effective
    ///   value as-is. Missing fields are rejected upstream by the
    ///   caller doing a `replace` of a partial body.
    /// - `Some(MergeMode::Patch)` with `ctx.old = Some(prior)`: shallow
    ///   merge `prior` with `new`, matching `sqlite::merge_patch` at
    ///   `crates/cosmix-lib-props-store/src/sqlite.rs:416`. The merged value
    ///   is then passed through [`fill_amendment_defaults`] so a Patch
    ///   over a pre-amendment v0.1.0 row sees the v0.2.0 DNSBL defaults
    ///   filled in before per-field validation runs (else a Patch from
    ///   a legacy client touching only the v0.1.0 fields would reject
    ///   on "DNSBL field missing"). Replace never goes through that
    ///   path — operators issuing Replace must carry the full 12-field
    ///   body.
    /// - `Some(MergeMode::Patch)` with `ctx.old = None`: rejected at
    ///   the caller; this function is never reached on that branch.
    /// - `None`: only on delete hooks; not reachable from `before_set`
    ///   after C0.
    fn effective_value(ctx: &HookCtx) -> Result<PropValue, HookError> {
        let merge = ctx.merge.ok_or_else(|| {
            HookError::hook(
                "engine_config before_set fired with merge=None \
                 (substrate regression — HookCtx.merge should be Some on set hooks)",
            )
        })?;
        let new = ctx
            .new
            .clone()
            .ok_or_else(|| HookError::hook("engine_config before_set fired with new=None"))?;
        match merge {
            MergeMode::Replace => Ok(new),
            MergeMode::Patch => {
                let old = ctx.old.clone().ok_or_else(|| {
                    HookError::validation(format!(
                        "{PATCH_REQUIRES_EXISTING_ROW_PREFIX} maild.engine_config has no prior row; \
                         use merge=replace with the full defaults body to initialise"
                    ))
                })?;
                Ok(fill_amendment_defaults(merge_patch_object(&old, new)))
            }
        }
    }

    /// Validate the *effective* value against the schema's semantic
    /// constraints (the schema enforces shape; this hook enforces
    /// finite-and-bounded numerics, the cross-field invariant, and the
    /// enum-list contents — `FieldType::Enum` is descriptive at the
    /// substrate level).
    fn validate(value: &PropValue) -> Result<(), HookError> {
        let obj = value.as_object().ok_or_else(|| {
            HookError::validation(format!(
                "engine_config record must be an object (got {})",
                value.type_name()
            ))
        })?;

        let threshold = require_f64(obj, "threshold", "Replace")?;
        let hard_junk = require_f64(obj, "hard_junk_threshold", "Replace")?;
        check_finite_nonneg_f32(threshold, "threshold")?;
        check_finite_nonneg_f32(hard_junk, "hard_junk_threshold")?;
        if threshold > 1000.0 {
            return Err(HookError::validation(format!(
                "threshold {threshold} exceeds 1000.0 ceiling"
            )));
        }
        if hard_junk > 1000.0 {
            return Err(HookError::validation(format!(
                "hard_junk_threshold {hard_junk} exceeds 1000.0 ceiling"
            )));
        }
        if hard_junk < threshold {
            return Err(HookError::validation(format!(
                "hard_junk_threshold {hard_junk} must be >= threshold {threshold}"
            )));
        }

        // shadow_mode is a bool — the schema accepts it without
        // additional semantic constraint; missing on Replace is the
        // "required field" check below.
        match obj.get("shadow_mode") {
            Some(PropValue::Bool(_)) => {}
            Some(other) => {
                return Err(HookError::validation(format!(
                    "shadow_mode must be a bool (got {})",
                    other.type_name()
                )));
            }
            None => {
                return Err(HookError::validation(format!(
                    "{REPLACE_REQUIRES_FULL_BODY_PREFIX} shadow_mode is required"
                )));
            }
        }

        check_u64_bounded(obj, "body_scan_bytes", 1, 67_108_864)?;
        check_u64_bounded(obj, "url_extraction_cap", 1, 1_000_000)?;
        check_u64_bounded(obj, "header_line_cap", 1, 100_000)?;
        check_u64_bounded(obj, "header_line_byte_cap", 1, 1_048_576)?;
        check_u64_bounded(obj, "budget_ms", 1, 600_000)?;

        // url_extraction_cap, header_line_cap, and budget_ms also need
        // to fit in u32 — caps above already enforce <= u32::MAX so
        // the engine narrow at C4-read time is safe.

        let kinds = obj.get("mail_auth_hard_fail_kinds").ok_or_else(|| {
            HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} mail_auth_hard_fail_kinds is required"
            ))
        })?;
        let entries = match kinds {
            PropValue::List(l) => l,
            other => {
                return Err(HookError::validation(format!(
                    "mail_auth_hard_fail_kinds must be a list (got {})",
                    other.type_name()
                )));
            }
        };
        for (i, entry) in entries.iter().enumerate() {
            match entry {
                PropValue::String(s) => {
                    if parse_mail_auth_kind(s).is_none() {
                        return Err(HookError::validation(format!(
                            "mail_auth_hard_fail_kinds[{i}] {s:?} is not a known variant \
                             (expected one of: SpfFailDmarcReject, DkimFailDmarcReject, BothAuthFailNoArc)"
                        )));
                    }
                }
                other => {
                    return Err(HookError::validation(format!(
                        "mail_auth_hard_fail_kinds[{i}] must be a string (got {})",
                        other.type_name()
                    )));
                }
            }
        }

        // v0.2.0 DNSBL amendment validators. Bounds mirror the
        // engine-domain ceilings: per-query timeout caps at 60 s (the
        // SMTP DATA stage is wall-clock-bound far below that anyway),
        // negative-TTL caps at 24 h (longer than that and the operator
        // is better off disabling DNSBL than holding stale negatives).
        check_u64_bounded(obj, "dnsbl_query_timeout_ms", 1, 60_000)?;
        check_u64_bounded(obj, "dnsbl_negative_ttl_secs", 1, 86_400)?;

        let addrs_field = obj.get("dnsbl_resolver_addresses").ok_or_else(|| {
            HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} dnsbl_resolver_addresses is required"
            ))
        })?;
        let addrs_list = match addrs_field {
            PropValue::List(l) => l,
            other => {
                return Err(HookError::validation(format!(
                    "dnsbl_resolver_addresses must be a list (got {})",
                    other.type_name()
                )));
            }
        };
        for (i, entry) in addrs_list.iter().enumerate() {
            let s = match entry {
                PropValue::String(s) => s,
                other => {
                    return Err(HookError::validation(format!(
                        "dnsbl_resolver_addresses[{i}] must be a string (got {})",
                        other.type_name()
                    )));
                }
            };
            if s.parse::<SocketAddr>().is_err() {
                return Err(HookError::validation(format!(
                    "dnsbl_resolver_addresses[{i}] {s:?} is not a valid 'ip:port' socket pair \
                     (hostnames are not accepted — use the resolved IP)"
                )));
            }
        }

        Ok(())
    }
}

impl HookHandler for EngineConfigHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let effective = Self::effective_value(ctx)?;
            Self::validate(&effective)
        })
    }

    fn after_set<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let Some(engine) = self.engine.get() else {
                // Engine handle not yet populated. Two legitimate
                // windows hit this branch:
                //   (a) Tests that exercise the substrate-side hook
                //       chain without an engine attached.
                //   (b) C4 bootstrap, between `register` and the
                //       `OnceLock::set` call in `main`. The defaults
                //       write that fires this hook then applies on
                //       the *next* well-formed write; the engine
                //       already holds whatever cfg `main` constructed
                //       it with, so the divergence is bounded.
                // In both cases the substrate row is durable, so a
                // no-op return is safe.
                return Ok(());
            };
            // Hold `apply_lock` across read-then-apply so two parallel
            // hooks cannot interleave and leave the engine observing
            // an earlier row after a later commit has already landed.
            let _guard = self.apply_lock.lock().await;
            let key = RecordKey::singleton(namespace_name());
            let snap = self.store.get(&key).await.map_err(|e| {
                HookError::hook(format!("engine_config after_set: read current row: {e}"))
            })?;
            let cfg = record_to_engine_config(&snap.value.value).map_err(|e| {
                HookError::hook(format!(
                    "engine_config after_set: validate current row: {e:#}"
                ))
            })?;
            engine.set_config(cfg).await;
            Ok(())
        })
    }

    fn before_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async {
            Err(HookError::validation(format!(
                "{UNDELETABLE_PREFIX} maild.engine_config is a required singleton; \
                 issue a `set merge=replace` with the defaults body to reset"
            )))
        })
    }
}

/// Shallow object-level merge mirroring the substrate's
/// `sqlite::merge_patch` (so the hook validates the same effective
/// value the storage layer will commit).
fn merge_patch_object(old: &PropValue, new: PropValue) -> PropValue {
    match (old, new) {
        (PropValue::Object(o), PropValue::Object(n)) => {
            let mut out = o.clone();
            for (k, v) in n {
                out.insert(k, v);
            }
            PropValue::Object(out)
        }
        (_, new) => new,
    }
}

/// Fill the v0.2.0 DNSBL amendment defaults into a merged-Patch object
/// when the underlying prior row predates the amendment. SPEC 12 §12
/// says a missing amendment-added field reads as its default; this
/// helper makes that explicit on the validation path so the per-field
/// checks downstream see a complete object.
///
/// Replace never calls this — operators issuing Replace must carry
/// the full 12-field body (and the hook rejects partial Replace below).
/// Only Patch runs through here, so a legacy operator script touching
/// only the v0.1.0 fields continues to work over a v0.1.0 row.
fn fill_amendment_defaults(value: PropValue) -> PropValue {
    let PropValue::Object(mut obj) = value else {
        // Not an object — let `validate` produce the typed error.
        return value;
    };
    let defaults = EngineConfig::default();
    obj.entry("dnsbl_query_timeout_ms".into())
        .or_insert(PropValue::UInt(u64::from(defaults.dnsbl_query_timeout_ms)));
    obj.entry("dnsbl_negative_ttl_secs".into())
        .or_insert(PropValue::UInt(u64::from(defaults.dnsbl_negative_ttl_secs)));
    obj.entry("dnsbl_resolver_addresses".into())
        .or_insert(PropValue::List(Vec::new()));
    PropValue::Object(obj)
}

fn require_f64(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    _ctx_label: &str,
) -> Result<f64, HookError> {
    match obj.get(field) {
        Some(PropValue::Float(f)) => Ok(*f),
        Some(PropValue::Int(n)) => Ok(*n as f64),
        Some(PropValue::UInt(n)) => Ok(*n as f64),
        Some(other) => Err(HookError::validation(format!(
            "{field} must be a number (got {})",
            other.type_name()
        ))),
        None => Err(HookError::validation(format!(
            "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
        ))),
    }
}

fn check_finite_nonneg_f32(v: f64, field: &str) -> Result<(), HookError> {
    if !v.is_finite() {
        return Err(HookError::validation(format!(
            "{field} {v} must be finite (no NaN or +/-inf)"
        )));
    }
    if v < 0.0 {
        return Err(HookError::validation(format!(
            "{field} {v} must be non-negative"
        )));
    }
    if !(v as f32).is_finite() {
        return Err(HookError::validation(format!(
            "{field} {v} overflows f32 range (engine reads as f32)"
        )));
    }
    Ok(())
}

fn check_u64_bounded(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    min: u64,
    max: u64,
) -> Result<(), HookError> {
    let v = match obj.get(field) {
        Some(PropValue::UInt(n)) => *n,
        Some(PropValue::Int(n)) => {
            if *n < 0 {
                return Err(HookError::validation(format!(
                    "{field} {n} must be non-negative"
                )));
            }
            *n as u64
        }
        Some(other) => {
            return Err(HookError::validation(format!(
                "{field} must be an unsigned integer (got {})",
                other.type_name()
            )));
        }
        None => {
            return Err(HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
            )));
        }
    };
    if v < min || v > max {
        return Err(HookError::validation(format!(
            "{field} {v} out of range ({min}..={max})"
        )));
    }
    Ok(())
}

/// Wire the `maild.engine_config` namespace into the substrate. Phase 3
/// C3 takes the engine `OnceLock` so `after_set` can reach the live
/// engine; `main` fills the lock after constructing the engine. The
/// lock may be empty at registration time and during the brief window
/// before `OnceLock::set` — `after_set` is a no-op until the lock is
/// populated (see [`EngineConfigHooks::after_set`] for the rationale).
pub fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    engine: Arc<OnceLock<Arc<DefaultRuleEngine>>>,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(EngineConfigHooks::new(engine, store.clone()));
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register engine_config namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register engine_config runtime on router: {e}"))?;
    Ok(runtime)
}

/// SPEC 12 Phase 3 C4 — startup-time read of the substrate row. Returns
/// the initial [`EngineConfig`] the engine should boot with plus a flag
/// indicating whether `main` still needs to materialise a defaults row
/// (true on a fresh install, false when a row already exists).
///
/// Called after [`register`] so the namespace metadata is staged, but
/// before the engine handle is constructed — so a corrupted on-disk row
/// surfaces as a typed startup error rather than as silently-feeding
/// out-of-band values into the engine.
pub async fn bootstrap_from_store(store: &Arc<SqliteStore>) -> Result<(EngineConfig, bool)> {
    let key = RecordKey::singleton(namespace_name());
    match store.get(&key).await {
        Ok(snap) => {
            let cfg = record_to_engine_config(&snap.value.value)
                .context("engine_config bootstrap: existing substrate row is invalid")?;
            Ok((cfg, false))
        }
        Err(StoreError::NotFound) => Ok((EngineConfig::default(), true)),
        Err(e) => Err(anyhow::anyhow!(
            "engine_config bootstrap: read substrate row: {e}"
        )),
    }
}

/// Read an `EngineConfig` back out of a substrate record value. Used by
/// the C3 converge-loop and the C4 bootstrap. Defensive against a
/// hand-edited or corrupted row — every field is re-validated against
/// the engine's domain (finite + bounded) so a bad row surfaces as an
/// `Err` rather than feeding `+inf` / negatives into the engine.
pub fn record_to_engine_config(value: &PropValue) -> Result<EngineConfig> {
    let obj = value.as_object().with_context(|| {
        format!(
            "engine_config record is not an object (got {})",
            value.type_name()
        )
    })?;

    // Engine-domain caps for the two float fields mirror `validate()`:
    // `<= 1000.0` ceiling on both. A corrupted row above the ceiling
    // would otherwise feed an out-of-band threshold straight into the
    // engine.
    let threshold = read_f32_bounded(obj, "threshold", 1000.0)?;
    let hard_junk = read_f32_bounded(obj, "hard_junk_threshold", 1000.0)?;
    if hard_junk < threshold {
        anyhow::bail!(
            "engine_config hard_junk_threshold {hard_junk} < threshold {threshold} (corrupted row?)"
        );
    }

    let shadow_mode = match obj.get("shadow_mode") {
        Some(PropValue::Bool(b)) => *b,
        Some(other) => {
            anyhow::bail!(
                "engine_config.shadow_mode has unexpected type {} (corrupted row?)",
                other.type_name()
            );
        }
        None => anyhow::bail!("engine_config.shadow_mode missing (corrupted row?)"),
    };

    // Mirror the write-side engine-domain caps from `validate()` on the
    // read path. A hand-edited or reconciled row that pre-dates a
    // tightened cap must surface as a typed read-side error rather than
    // silently feeding the engine. Each read also guards explicit
    // target-type overflow (u64 → u32 / u64 → usize) so a future
    // cap relaxation can't silently truncate.
    let body_scan_bytes_u64 = read_u64_bounded(obj, "body_scan_bytes", 1, 67_108_864)?;
    let url_extraction_cap = read_u64_bounded(obj, "url_extraction_cap", 1, 1_000_000)?;
    let header_line_cap = read_u64_bounded(obj, "header_line_cap", 1, 100_000)?;
    let header_line_byte_cap_u64 = read_u64_bounded(obj, "header_line_byte_cap", 1, 1_048_576)?;
    let budget_ms = read_u64_bounded(obj, "budget_ms", 1, 600_000)?;

    if url_extraction_cap > u64::from(u32::MAX) {
        anyhow::bail!("engine_config.url_extraction_cap {url_extraction_cap} overflows u32");
    }
    if header_line_cap > u64::from(u32::MAX) {
        anyhow::bail!("engine_config.header_line_cap {header_line_cap} overflows u32");
    }
    if budget_ms > u64::from(u32::MAX) {
        anyhow::bail!("engine_config.budget_ms {budget_ms} overflows u32");
    }
    let body_scan_bytes = usize::try_from(body_scan_bytes_u64).map_err(|_| {
        anyhow::anyhow!("engine_config.body_scan_bytes {body_scan_bytes_u64} overflows usize")
    })?;
    let header_line_byte_cap = usize::try_from(header_line_byte_cap_u64).map_err(|_| {
        anyhow::anyhow!(
            "engine_config.header_line_byte_cap {header_line_byte_cap_u64} overflows usize"
        )
    })?;

    let kinds_field = obj
        .get("mail_auth_hard_fail_kinds")
        .ok_or_else(|| anyhow::anyhow!("engine_config.mail_auth_hard_fail_kinds missing"))?;
    let kinds_list = match kinds_field {
        PropValue::List(l) => l,
        other => anyhow::bail!(
            "engine_config.mail_auth_hard_fail_kinds is not a list (got {})",
            other.type_name()
        ),
    };
    let mut kinds = Vec::with_capacity(kinds_list.len());
    for (i, entry) in kinds_list.iter().enumerate() {
        let s = match entry {
            PropValue::String(s) => s,
            other => anyhow::bail!(
                "engine_config.mail_auth_hard_fail_kinds[{i}] is not a string (got {})",
                other.type_name()
            ),
        };
        let kind = parse_mail_auth_kind(s).ok_or_else(|| {
            anyhow::anyhow!(
                "engine_config.mail_auth_hard_fail_kinds[{i}] {s:?} is not a known variant"
            )
        })?;
        kinds.push(kind);
    }

    // v0.2.0 DNSBL amendment — read-default semantics (SPEC 12 §12).
    // A pre-amendment row writes these fields absent; we fill from
    // `EngineConfig::default()` so the engine sees the v0.2.0 contract
    // regardless of whether the operator has yet issued a Replace to
    // upgrade the row on disk. Once the row is written via Replace
    // (e.g. by the bootstrap path on a fresh install, or by an explicit
    // operator action) the absent branch never fires again.
    let defaults = EngineConfig::default();
    let dnsbl_query_timeout_ms = read_u64_bounded_or(
        obj,
        "dnsbl_query_timeout_ms",
        1,
        60_000,
        u64::from(defaults.dnsbl_query_timeout_ms),
    )?;
    let dnsbl_negative_ttl_secs = read_u64_bounded_or(
        obj,
        "dnsbl_negative_ttl_secs",
        1,
        86_400,
        u64::from(defaults.dnsbl_negative_ttl_secs),
    )?;
    if dnsbl_query_timeout_ms > u64::from(u32::MAX) {
        anyhow::bail!(
            "engine_config.dnsbl_query_timeout_ms {dnsbl_query_timeout_ms} overflows u32"
        );
    }
    if dnsbl_negative_ttl_secs > u64::from(u32::MAX) {
        anyhow::bail!(
            "engine_config.dnsbl_negative_ttl_secs {dnsbl_negative_ttl_secs} overflows u32"
        );
    }
    let dnsbl_resolver_addresses = read_socket_addr_list(obj, "dnsbl_resolver_addresses")?;

    Ok(EngineConfig {
        threshold,
        hard_junk_threshold: hard_junk,
        shadow_mode,
        body_scan_bytes,
        url_extraction_cap: url_extraction_cap as u32,
        header_line_cap: header_line_cap as u32,
        header_line_byte_cap,
        budget_ms: budget_ms as u32,
        mail_auth_hard_fail_kinds: kinds,
        dnsbl_query_timeout_ms: dnsbl_query_timeout_ms as u32,
        dnsbl_negative_ttl_secs: dnsbl_negative_ttl_secs as u32,
        dnsbl_resolver_addresses,
    })
}

/// Read an F32 field and enforce the engine-domain `0.0..=max` range
/// (mirror of `check_finite_nonneg_f32` + the explicit `<= max` cap on
/// the write path). A finite f64 above f32 range narrows to +inf and
/// is also caught.
fn read_f32_bounded(obj: &BTreeMap<String, PropValue>, field: &str, max: f32) -> Result<f32> {
    let v = match obj.get(field) {
        Some(PropValue::Float(f)) => *f,
        Some(PropValue::Int(n)) => *n as f64,
        Some(PropValue::UInt(n)) => *n as f64,
        Some(other) => anyhow::bail!(
            "engine_config.{field} has unexpected type {}",
            other.type_name()
        ),
        None => anyhow::bail!("engine_config.{field} missing"),
    };
    if !v.is_finite() || v < 0.0 {
        anyhow::bail!("engine_config.{field} {v} violates engine domain (corrupted row?)");
    }
    let narrowed = v as f32;
    if !narrowed.is_finite() {
        anyhow::bail!("engine_config.{field} {v} overflows f32 range (corrupted row?)");
    }
    if narrowed > max {
        anyhow::bail!(
            "engine_config.{field} {narrowed} exceeds engine-domain ceiling {max} (corrupted row?)"
        );
    }
    Ok(narrowed)
}

/// Read a U64 field with read-default fallback for amendment-added
/// fields (SPEC 12 §12). A pre-amendment row that omits the field
/// reads as `default`; a present value is bounds-checked the same as
/// `read_u64_bounded`. Used for the v0.2.0 DNSBL knobs so a sparse
/// pre-amendment row continues to boot.
fn read_u64_bounded_or(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    min: u64,
    max: u64,
    default: u64,
) -> Result<u64> {
    if !obj.contains_key(field) {
        return Ok(default);
    }
    read_u64_bounded(obj, field, min, max)
}

/// Read a list-of-strings field and parse each entry as a SocketAddr
/// (`ip:port`). Empty / missing list returns `Vec::new()` — the
/// "missing" branch is the read-default for the v0.2.0 amendment
/// (SPEC 12 §12). A present-but-malformed entry is a typed corruption
/// error: `validate()` rejects this on the write path, so the read
/// helper only catches hand-edits / pre-validator rows.
fn read_socket_addr_list(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
) -> Result<Vec<SocketAddr>> {
    let entry = match obj.get(field) {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    let list = match entry {
        PropValue::List(l) => l,
        other => anyhow::bail!(
            "engine_config.{field} is not a list (got {})",
            other.type_name()
        ),
    };
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        let s = match item {
            PropValue::String(s) => s,
            other => anyhow::bail!(
                "engine_config.{field}[{i}] is not a string (got {})",
                other.type_name()
            ),
        };
        let addr = s.parse::<SocketAddr>().map_err(|e| {
            anyhow::anyhow!(
                "engine_config.{field}[{i}] {s:?} is not a valid 'ip:port' socket pair: {e}"
            )
        })?;
        out.push(addr);
    }
    Ok(out)
}

/// Read a U64 field and enforce the engine-domain `[min, max]` range
/// (mirror of `check_u64_bounded` on the write path). Used by
/// `record_to_engine_config` so a corrupted row whose value pre-dates
/// the current cap or violates the engine's lower bound surfaces as a
/// typed read-side error rather than feeding the engine.
fn read_u64_bounded(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    min: u64,
    max: u64,
) -> Result<u64> {
    let v = match obj.get(field) {
        Some(PropValue::UInt(n)) => *n,
        Some(PropValue::Int(n)) => {
            if *n < 0 {
                anyhow::bail!("engine_config.{field} {n} is negative (corrupted row?)");
            }
            *n as u64
        }
        Some(other) => anyhow::bail!(
            "engine_config.{field} has unexpected type {}",
            other.type_name()
        ),
        None => anyhow::bail!("engine_config.{field} missing"),
    };
    if v < min || v > max {
        anyhow::bail!(
            "engine_config.{field} {v} outside engine-domain {min}..={max} (corrupted row?)"
        );
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, RecordKey, Version};
    use rusqlite::Connection;

    fn key() -> RecordKey {
        RecordKey::singleton(namespace_name())
    }

    /// Hook handler wired with a fresh in-memory `SqliteStore` and an
    /// empty engine `OnceLock`. The unit-level `before_set` /
    /// `before_delete` tests only exercise validation logic so the
    /// engine field is irrelevant; the store handle is mandatory by
    /// type, not by behaviour, and the integration-level tests below
    /// use a richer setup that registers the namespace and populates
    /// the engine lock.
    fn test_hooks() -> EngineConfigHooks {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        EngineConfigHooks::new(Arc::new(OnceLock::new()), store)
    }

    fn set_ctx(merge: MergeMode, old: Option<PropValue>, new: PropValue) -> HookCtx {
        HookCtx {
            key: key(),
            old,
            new: Some(new),
            version: Version::zero(),
            actor: Actor::service("test").expect("valid actor"),
            merge: Some(merge),
            origin: cosmix_props::WriteOrigin::caller(),
        }
    }

    fn delete_ctx() -> HookCtx {
        HookCtx {
            key: key(),
            old: Some(defaults_value()),
            new: None,
            version: Version::zero(),
            actor: Actor::service("test").expect("valid actor"),
            merge: None,
            origin: cosmix_props::WriteOrigin::caller(),
        }
    }

    fn defaults_obj_mut(mut f: impl FnMut(&mut BTreeMap<String, PropValue>)) -> PropValue {
        let mut v = defaults_value();
        match &mut v {
            PropValue::Object(m) => f(m),
            _ => unreachable!(),
        }
        v
    }

    // Test 1: schema/value round-trip — defaults_value parses back to defaults.
    #[tokio::test]
    async fn schema_round_trip_defaults() {
        let v = defaults_value();
        let cfg = record_to_engine_config(&v).expect("defaults round-trip");
        let expected = EngineConfig::default();
        assert_eq!(cfg.threshold, expected.threshold);
        assert_eq!(cfg.hard_junk_threshold, expected.hard_junk_threshold);
        assert_eq!(cfg.shadow_mode, expected.shadow_mode);
        assert_eq!(cfg.body_scan_bytes, expected.body_scan_bytes);
        assert_eq!(cfg.url_extraction_cap, expected.url_extraction_cap);
        assert_eq!(cfg.header_line_cap, expected.header_line_cap);
        assert_eq!(cfg.header_line_byte_cap, expected.header_line_byte_cap);
        assert_eq!(cfg.budget_ms, expected.budget_ms);
        assert_eq!(
            cfg.mail_auth_hard_fail_kinds,
            expected.mail_auth_hard_fail_kinds
        );
    }

    // Test 3: before_set rejects NaN / +inf / -inf / negative threshold.
    #[tokio::test]
    async fn before_set_rejects_nonfinite_threshold() {
        let hooks = test_hooks();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5] {
            let v = defaults_obj_mut(|m| {
                m.insert("threshold".into(), PropValue::Float(bad));
            });
            let err = hooks
                .before_set(&set_ctx(MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(matches!(err, HookError::Validation { .. }), "bad={bad}");
        }
    }

    // Test 4: before_set rejects f64 > f32::MAX (post-narrow non-finite).
    #[tokio::test]
    async fn before_set_rejects_f64_above_f32_max() {
        let hooks = test_hooks();
        // 1e39 is finite as f64 (< f64::MAX ~1.8e308) but overflows f32::MAX (~3.4e38).
        let huge = 1.0e39_f64;
        assert!(huge.is_finite());
        assert!(!(huge as f32).is_finite());
        let v = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(huge));
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        let msg = err.message();
        assert!(msg.contains("f32"), "expected f32-range error, got: {msg}");
    }

    // Test 5: before_set rejects hard_junk_threshold < threshold (cross-field).
    #[tokio::test]
    async fn before_set_rejects_cross_field_invariant() {
        let hooks = test_hooks();
        let v = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(10.0));
            m.insert("hard_junk_threshold".into(), PropValue::Float(5.0));
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains(">= threshold"));
    }

    // Test 6: before_set rejects each U64 field above its ceiling.
    #[tokio::test]
    async fn before_set_rejects_u64_above_ceiling() {
        let hooks = test_hooks();
        let cases: &[(&str, u64)] = &[
            ("body_scan_bytes", 67_108_865),
            ("url_extraction_cap", 1_000_001),
            ("header_line_cap", 100_001),
            ("header_line_byte_cap", 1_048_577),
            ("budget_ms", 600_001),
        ];
        for (field, bad) in cases {
            let v = defaults_obj_mut(|m| {
                m.insert((*field).into(), PropValue::UInt(*bad));
            });
            let err = hooks
                .before_set(&set_ctx(MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(
                err.message().contains(field) && err.message().contains("out of range"),
                "{field}={bad} should reject, got: {}",
                err.message()
            );
        }
    }

    // Test 7: before_set rejects unknown mail_auth_hard_fail_kinds entries.
    #[tokio::test]
    async fn before_set_rejects_unknown_mail_auth_variant() {
        let hooks = test_hooks();
        let v = defaults_obj_mut(|m| {
            m.insert(
                "mail_auth_hard_fail_kinds".into(),
                PropValue::List(vec![PropValue::String("MadeUpVariant".into())]),
            );
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("not a known variant"));
    }

    // Test 8: before_delete rejects with engine_config_undeletable: prefix.
    #[tokio::test]
    async fn before_delete_rejects_with_prefix() {
        let hooks = test_hooks();
        let err = hooks.before_delete(&delete_ctx()).await.unwrap_err();
        assert!(err.message().starts_with(UNDELETABLE_PREFIX));
    }

    // Test 9: MergeMode::Patch over an absent row is rejected.
    #[tokio::test]
    async fn before_set_patch_over_absent_rejects() {
        let hooks = test_hooks();
        let patch = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(3.0));
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Patch, None, patch))
            .await
            .unwrap_err();
        assert!(
            err.message()
                .starts_with(PATCH_REQUIRES_EXISTING_ROW_PREFIX),
            "got: {}",
            err.message()
        );
    }

    // Test 10: MergeMode::Replace accepts a full-shape body.
    #[tokio::test]
    async fn before_set_replace_accepts_full_body() {
        let hooks = test_hooks();
        hooks
            .before_set(&set_ctx(MergeMode::Replace, None, defaults_value()))
            .await
            .expect("defaults body should pass Replace");
    }

    /// Build a v0.1.0 (pre-DNSBL-amendment) row by serialising the
    /// defaults then stripping the three v0.2.0 fields. Models a row
    /// written before the amendment landed.
    fn v01_row() -> PropValue {
        defaults_obj_mut(|m| {
            m.remove("dnsbl_query_timeout_ms");
            m.remove("dnsbl_negative_ttl_secs");
            m.remove("dnsbl_resolver_addresses");
        })
    }

    /// Patch from a legacy client touches only a v0.1.0 field over a
    /// v0.1.0 row. The hook must accept it: the merged value is missing
    /// the v0.2.0 fields, `fill_amendment_defaults` fills the defaults,
    /// and per-field validation passes on the complete object.
    #[tokio::test]
    async fn before_set_patch_over_v01_row_with_amendment_defaults() {
        let hooks = test_hooks();
        let patch = PropValue::Object({
            let mut m = BTreeMap::new();
            m.insert("threshold".into(), PropValue::Float(7.5));
            m
        });
        hooks
            .before_set(&set_ctx(MergeMode::Patch, Some(v01_row()), patch))
            .await
            .expect("legacy Patch over v0.1.0 row should accept the amendment defaults");
    }

    /// A Patch that touches one of the new DNSBL fields with an
    /// out-of-bounds value still rejects — read-default is only the
    /// "missing field" branch.
    #[tokio::test]
    async fn before_set_patch_rejects_out_of_bounds_dnsbl_field() {
        let hooks = test_hooks();
        let patch = PropValue::Object({
            let mut m = BTreeMap::new();
            m.insert("dnsbl_query_timeout_ms".into(), PropValue::UInt(60_001));
            m
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Patch, Some(defaults_value()), patch))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("dnsbl_query_timeout_ms")
                && err.message().contains("out of range"),
            "expected dnsbl_query_timeout_ms range error, got: {}",
            err.message()
        );
    }

    /// Replace MUST carry every field — including the v0.2.0 DNSBL
    /// fields. Old clients doing Replace over a v0.1.0 body get the
    /// "required field" rejection so they must learn the amendment
    /// before they can ship a replacement body.
    #[tokio::test]
    async fn before_set_replace_rejects_partial_v01_body() {
        let hooks = test_hooks();
        let err = hooks
            .before_set(&set_ctx(MergeMode::Replace, None, v01_row()))
            .await
            .unwrap_err();
        assert!(
            err.message().starts_with(REPLACE_REQUIRES_FULL_BODY_PREFIX)
                && err.message().contains("dnsbl_"),
            "expected required-field error on DNSBL field, got: {}",
            err.message()
        );
    }

    /// Malformed socket-addr strings reject. SocketAddr parser requires
    /// `ip:port`; hostnames (even with a port) are rejected so the
    /// daemon does not have to do DNS at validate time.
    #[tokio::test]
    async fn before_set_rejects_malformed_resolver_address() {
        let hooks = test_hooks();
        for bad in ["", "1.2.3.4", "1.2.3.4:port", "example.com:53", "::1:53"] {
            let v = defaults_obj_mut(|m| {
                m.insert(
                    "dnsbl_resolver_addresses".into(),
                    PropValue::List(vec![PropValue::String(bad.into())]),
                );
            });
            let err = hooks
                .before_set(&set_ctx(MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(
                err.message().contains("dnsbl_resolver_addresses")
                    && err.message().contains("ip:port"),
                "bad={bad:?} expected ip:port error, got: {}",
                err.message()
            );
        }
    }

    /// Valid `ip:port` (IPv4 and IPv6 bracketed) entries round-trip
    /// through the read path.
    #[tokio::test]
    async fn dnsbl_resolver_addresses_round_trip() {
        let cfg = EngineConfig {
            dnsbl_resolver_addresses: vec![
                "1.1.1.1:53".parse().unwrap(),
                "[2001:4860:4860::8888]:53".parse().unwrap(),
            ],
            ..EngineConfig::default()
        };
        let v = engine_config_to_value(&cfg);
        let read_back = record_to_engine_config(&v).expect("round-trip");
        assert_eq!(
            read_back.dnsbl_resolver_addresses,
            cfg.dnsbl_resolver_addresses
        );
    }

    /// `record_to_engine_config` over a v0.1.0 row (no DNSBL fields)
    /// returns an EngineConfig with the amendment defaults filled. This
    /// is the on-boot read path for a substrate row that pre-dates the
    /// amendment — the engine boots as if the row carried the defaults.
    #[test]
    fn record_read_fills_amendment_defaults_for_v01_row() {
        let v = v01_row();
        let cfg = record_to_engine_config(&v).expect("v0.1.0 row should read with defaults");
        let defaults = EngineConfig::default();
        assert_eq!(cfg.dnsbl_query_timeout_ms, defaults.dnsbl_query_timeout_ms);
        assert_eq!(
            cfg.dnsbl_negative_ttl_secs,
            defaults.dnsbl_negative_ttl_secs
        );
        assert!(cfg.dnsbl_resolver_addresses.is_empty());
    }

    /// A present-but-malformed `dnsbl_resolver_addresses` entry on the
    /// read path surfaces as a typed error (not a panic, not a silent
    /// drop). `validate` rejects this on the write path; the read
    /// helper only catches hand-edits / migrated rows.
    #[test]
    fn record_read_rejects_malformed_resolver_address() {
        let v = defaults_obj_mut(|m| {
            m.insert(
                "dnsbl_resolver_addresses".into(),
                PropValue::List(vec![PropValue::String("nope".into())]),
            );
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("dnsbl_resolver_addresses") && err.contains("ip:port"),
            "expected ip:port error, got: {err}"
        );
    }

    // Test 10a: MergeMode::Patch over existing validates the merged effective
    // value — a patch that would create an invalid cross-field state is rejected.
    #[tokio::test]
    async fn before_set_patch_over_existing_validates_merged_value() {
        let hooks = test_hooks();
        // Prior row at defaults: threshold=5.0, hard_junk_threshold=15.0.
        // Patch raises threshold to 20.0 (above 15.0) without touching hard_junk_threshold.
        // The merged effective value violates the cross-field invariant.
        let patch = PropValue::Object({
            let mut m = BTreeMap::new();
            m.insert("threshold".into(), PropValue::Float(20.0));
            m
        });
        let err = hooks
            .before_set(&set_ctx(MergeMode::Patch, Some(defaults_value()), patch))
            .await
            .unwrap_err();
        assert!(
            err.message().contains(">= threshold"),
            "expected cross-field rejection on merged value, got: {}",
            err.message()
        );
    }

    // Test 2 (defaults bootstrap) is exercised at integration level in C4
    // (the bootstrap path is in main.rs); the unit-level "schema round-trip"
    // test above covers the value-shape half.

    #[test]
    fn schema_lists_twelve_fields() {
        let s = schema();
        let names: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "threshold",
                "hard_junk_threshold",
                "shadow_mode",
                "body_scan_bytes",
                "url_extraction_cap",
                "header_line_cap",
                "header_line_byte_cap",
                "budget_ms",
                "mail_auth_hard_fail_kinds",
                "dnsbl_query_timeout_ms",
                "dnsbl_negative_ttl_secs",
                "dnsbl_resolver_addresses",
            ]
        );
    }

    /// v0.2.0 amendment fields carry a `since:` marker; the v0.1.0
    /// originals stay `since: None`. A substrate client reading the
    /// schema uses this to decide whether absent-in-record reads as
    /// "missing required field" (v0.1.0) or "fill from default" (v0.2.0).
    #[test]
    fn dnsbl_fields_carry_amendment_since_marker() {
        let s = schema();
        let by_name: BTreeMap<&str, &FieldSchema> =
            s.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        for v01_field in [
            "threshold",
            "hard_junk_threshold",
            "shadow_mode",
            "body_scan_bytes",
            "url_extraction_cap",
            "header_line_cap",
            "header_line_byte_cap",
            "budget_ms",
            "mail_auth_hard_fail_kinds",
        ] {
            assert!(
                by_name[v01_field].since.is_none(),
                "v0.1.0 field {v01_field} should carry no since marker"
            );
        }
        for v02_field in [
            "dnsbl_query_timeout_ms",
            "dnsbl_negative_ttl_secs",
            "dnsbl_resolver_addresses",
        ] {
            assert_eq!(
                by_name[v02_field].since,
                Some(DNSBL_AMENDMENT_VERSION),
                "v0.2.0 field {v02_field} must carry the DNSBL_AMENDMENT_VERSION marker"
            );
        }
    }

    #[test]
    fn cardinality_is_singleton_current() {
        let s = spec(Hooks::noop());
        match &s.cardinality {
            Cardinality::Singleton { canonical_key } => {
                assert_eq!(canonical_key, CANONICAL_KEY);
            }
            other => panic!("expected Singleton, got {other:?}"),
        }
    }

    #[test]
    fn auth_policy_grants_full_capability_set() {
        let policy = auth_policy();
        let caps = policy.resolve(&PeerIdentity::default());
        for c in [
            "props.read:maild.engine_config",
            "props.write:maild.engine_config",
            "props.describe:maild.engine_config:public",
            "props.describe:maild.engine_config:full",
            "props.audit:maild.engine_config",
        ] {
            assert!(
                caps.contains(&Capability::new(c).expect("non-empty capability")),
                "cap {c} granted"
            );
        }
    }

    #[test]
    fn spec_carries_amendment_since_version() {
        // SPEC 12 §12 — the namespace's `since_version` advertises the
        // current schema version. When the DNSBL amendment lands the
        // namespace transitions from 0.1.0 → 0.2.0; clients reading the
        // `describe` projection see `schema_version: "0.2.0"` and know
        // the v0.2.0 fields are part of the contract.
        let s = spec(Hooks::noop());
        assert_eq!(s.since_version, DNSBL_AMENDMENT_VERSION);
    }

    #[test]
    fn spec_requires_version() {
        // SPEC 12 §4.4 — closes the hook/commit TOCTOU. Every
        // `props.set engine_config` must carry `if_version`. C4
        // bootstrap supplies `Version::zero()` for create-if-absent.
        let s = spec(Hooks::noop());
        assert!(
            s.require_version,
            "engine_config spec must set require_version=true"
        );
    }

    // ── record_to_engine_config defensive read-side tests (MAJOR #2) ──
    // These rows would have been rejected by `before_set`'s validators,
    // but if a hand-edit, reconcile, or pre-cap migration leaves a bad
    // row in the substrate, the reader must surface a typed error so
    // C3/C4 do not feed a poisoned config into the engine.

    #[test]
    fn record_read_rejects_nonfinite_threshold() {
        let v = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(f64::NAN));
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("threshold") && err.contains("engine domain"),
            "expected engine-domain error, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_threshold_above_f32_max() {
        let v = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(1.0e40));
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("threshold") && err.contains("overflows f32"),
            "expected f32 overflow error, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_cross_field_invariant() {
        let v = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(5.0));
            m.insert("hard_junk_threshold".into(), PropValue::Float(3.0));
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("hard_junk_threshold") && err.contains("threshold"),
            "expected cross-field error, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_unknown_mail_auth_variant() {
        let v = defaults_obj_mut(|m| {
            m.insert(
                "mail_auth_hard_fail_kinds".into(),
                PropValue::List(vec![PropValue::String("NotAKind".into())]),
            );
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("not a known variant"),
            "expected unknown-variant error, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_non_list_mail_auth_kinds() {
        let v = defaults_obj_mut(|m| {
            m.insert(
                "mail_auth_hard_fail_kinds".into(),
                PropValue::String("SpfFailDmarcReject".into()),
            );
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("is not a list"),
            "expected non-list error, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_u64_zero() {
        // Every U64 field has a lower bound of 1 on the write path.
        // Zero in a stored row is the corruption signature to catch.
        for field in [
            "body_scan_bytes",
            "url_extraction_cap",
            "header_line_cap",
            "header_line_byte_cap",
            "budget_ms",
        ] {
            let v = defaults_obj_mut(|m| {
                m.insert(field.into(), PropValue::UInt(0));
            });
            let err = record_to_engine_config(&v).unwrap_err().to_string();
            assert!(
                err.contains(field) && err.contains("outside engine-domain"),
                "expected engine-domain error for {field}, got: {err}"
            );
        }
    }

    #[test]
    fn record_read_rejects_u64_above_cap() {
        // One representative per cap; the helper is uniform across fields.
        let cases = [
            ("body_scan_bytes", 67_108_865_u64),
            ("url_extraction_cap", 1_000_001_u64),
            ("header_line_cap", 100_001_u64),
            ("header_line_byte_cap", 1_048_577_u64),
            ("budget_ms", 600_001_u64),
        ];
        for (field, over) in cases {
            let v = defaults_obj_mut(|m| {
                m.insert(field.into(), PropValue::UInt(over));
            });
            let err = record_to_engine_config(&v).unwrap_err().to_string();
            assert!(
                err.contains(field) && err.contains("outside engine-domain"),
                "expected over-cap error for {field}, got: {err}"
            );
        }
    }

    #[test]
    fn record_read_rejects_threshold_above_ceiling() {
        // Write-side `validate()` rejects threshold > 1000.0; read side
        // must too, else a corrupted row above the ceiling feeds the
        // engine an out-of-band threshold. threshold is read first, so
        // exercise hard_junk_threshold by keeping threshold in-band.
        let v_threshold = defaults_obj_mut(|m| {
            m.insert("threshold".into(), PropValue::Float(1001.0));
            // Keep hard_junk >= threshold so the cross-field check
            // doesn't preempt the ceiling check.
            m.insert("hard_junk_threshold".into(), PropValue::Float(1001.0));
        });
        let err = record_to_engine_config(&v_threshold)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("threshold") && err.contains("ceiling"),
            "expected ceiling error for threshold, got: {err}"
        );

        let v_hard = defaults_obj_mut(|m| {
            // threshold stays at the defaults' 5.0; hard_junk above cap.
            m.insert("hard_junk_threshold".into(), PropValue::Float(1001.0));
        });
        let err = record_to_engine_config(&v_hard).unwrap_err().to_string();
        assert!(
            err.contains("hard_junk_threshold") && err.contains("ceiling"),
            "expected ceiling error for hard_junk_threshold, got: {err}"
        );
    }

    #[test]
    fn record_read_rejects_negative_int_u64() {
        let v = defaults_obj_mut(|m| {
            m.insert("body_scan_bytes".into(), PropValue::Int(-1));
        });
        let err = record_to_engine_config(&v).unwrap_err().to_string();
        assert!(
            err.contains("body_scan_bytes") && err.contains("negative"),
            "expected negative-int error, got: {err}"
        );
    }

    // ── C3 integration: after_set → engine.set_config wiring ──────

    use cosmix_maild_rules::DefaultRuleEngine;
    use cosmix_props::runtime::SetOpts;

    /// Build a registered engine_config runtime + the live engine the
    /// after_set hook reconfigures. Returned tuple: (runtime, engine).
    /// The engine boots from `EngineConfig::default()` and the OnceLock
    /// is filled before `register` so the very first hook fires against
    /// a wired engine.
    fn end_to_end_rig() -> (Arc<Runtime>, Arc<DefaultRuleEngine>) {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");

        let (engine_inner, _failures) = DefaultRuleEngine::with_pack_str(
            EngineConfig::default(),
            cosmix_maild_rules::default_pack_str(),
        )
        .expect("pack load");
        let engine = Arc::new(engine_inner);

        let engine_lock = Arc::new(OnceLock::new());
        engine_lock.set(engine.clone()).ok().expect("set lock");

        let runtime = register(&mut router, &store, engine_lock).expect("engine_config register");

        (runtime, engine)
    }

    fn set_opts(version: Option<Version>) -> SetOpts {
        SetOpts {
            expected_version: version,
            merge: MergeMode::Replace,
            actor: Actor::operator("test").expect("valid actor"),
            cause: None,
            ts_ms: 0,
        }
    }

    /// Test 11: a Replace through the substrate runtime propagates to
    /// the live engine — both `shadow_mode` and `hard_junk_threshold`
    /// must be observable on the engine immediately after `set` returns.
    #[tokio::test]
    async fn end_to_end_set_propagates_to_engine() {
        let (runtime, engine) = end_to_end_rig();

        // Pre-state: engine boots at defaults.
        let pre = engine.config_snapshot().await;
        assert!(!pre.shadow_mode);
        assert_eq!(
            pre.hard_junk_threshold,
            EngineConfig::default().hard_junk_threshold
        );

        // Build a defaults body that flips shadow_mode on and raises
        // hard_junk_threshold so the change is observable.
        let body = engine_config_to_value(&EngineConfig {
            shadow_mode: true,
            hard_junk_threshold: 22.5,
            ..EngineConfig::default()
        });

        // First-time Replace — no prior row, so expected_version is
        // Version::zero() (create-if-absent).
        let outcome = runtime
            .set(key(), body, set_opts(Some(Version::zero())))
            .await
            .expect("first Replace succeeds");
        assert!(
            outcome.warnings.is_empty(),
            "after_set should not produce warnings on the happy path, got: {:?}",
            outcome.warnings
        );

        // Post-state: engine snapshot reflects the new row.
        let post = engine.config_snapshot().await;
        assert!(
            post.shadow_mode,
            "shadow_mode should be true after substrate set"
        );
        assert_eq!(post.hard_junk_threshold, 22.5);
    }

    /// Test 12: storm of writes converges to the substrate's final
    /// row. Each write supplies the previously-observed version so the
    /// substrate's `require_version=true` serializes commits; the
    /// apply_lock serializes hooks; the last commit's hook leaves the
    /// engine matching the substrate row.
    #[tokio::test]
    async fn end_to_end_storm_of_writes_converges() {
        let (runtime, engine) = end_to_end_rig();

        // Bootstrap the row at known state so subsequent Patches have
        // a prior to merge with.
        let initial = engine_config_to_value(&EngineConfig::default());
        let first = runtime
            .set(key(), initial, set_opts(Some(Version::zero())))
            .await
            .expect("bootstrap row");

        // Walk the row through 20 distinct hard_junk_threshold values,
        // each via a Patch over the prior. After the loop, the engine
        // and the substrate must both read the final value.
        let mut version = first.set_event.version;
        let mut next_hj = 20.0_f32;
        for _ in 0..20 {
            let patch = PropValue::Object({
                let mut m = BTreeMap::new();
                m.insert(
                    "hard_junk_threshold".into(),
                    PropValue::Float(f64::from(next_hj)),
                );
                m
            });
            let outcome = runtime
                .set(
                    key(),
                    patch,
                    SetOpts {
                        expected_version: Some(version),
                        merge: MergeMode::Patch,
                        actor: Actor::operator("test").expect("valid actor"),
                        cause: None,
                        ts_ms: 0,
                    },
                )
                .await
                .expect("patch succeeds");
            assert!(
                outcome.warnings.is_empty(),
                "after_set should not warn during converge storm: {:?}",
                outcome.warnings
            );
            version = outcome.set_event.version;
            next_hj += 1.0;
        }

        // Final expected hard_junk_threshold is 20.0 + 19.0 = 39.0
        // (loop ran 20 iterations starting at 20.0).
        let final_hj = 39.0_f32;
        let snap = engine.config_snapshot().await;
        assert_eq!(
            snap.hard_junk_threshold, final_hj,
            "engine final hard_junk_threshold should match the last commit"
        );

        let store_snap = runtime.store().get(&key()).await.expect("read final row");
        let store_cfg = record_to_engine_config(&store_snap.value.value).expect("validate final");
        assert_eq!(
            store_cfg.hard_junk_threshold, final_hj,
            "substrate row should match the loop's last commit"
        );
    }

    /// Test 12b: a hook firing without a wired engine returns Ok (the
    /// no-op branch). Exercises the OnceLock-empty window — relevant
    /// to C4 bootstrap, where the substrate row may be written before
    /// the engine handle is filled in.
    #[tokio::test]
    async fn after_set_is_noop_when_engine_lock_empty() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");

        // Empty lock — never populated.
        let engine_lock = Arc::new(OnceLock::new());
        let runtime = register(&mut router, &store, engine_lock).expect("engine_config register");

        // A well-formed Replace must succeed with no warnings (after_set
        // returns Ok early because the OnceLock is empty).
        let body = engine_config_to_value(&EngineConfig::default());
        let outcome = runtime
            .set(key(), body, set_opts(Some(Version::zero())))
            .await
            .expect("first Replace succeeds");
        assert!(
            outcome.warnings.is_empty(),
            "after_set should be a silent no-op when engine lock is empty, got: {:?}",
            outcome.warnings
        );
    }

    // ── C4 bootstrap: bootstrap_from_store ────────────────────────

    /// Test 13: fresh install — no row yet. Returns
    /// `(EngineConfig::default(), true)` so `main` materialises the
    /// defaults row after filling the engine OnceLock.
    #[tokio::test]
    async fn bootstrap_from_store_on_empty_store_yields_defaults_and_materialise_flag() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");

        // Register the namespace so substrate metadata is staged but no
        // row exists yet — matches main.rs ordering.
        let engine_lock = Arc::new(OnceLock::new());
        let _runtime = register(&mut router, &store, engine_lock).expect("engine_config register");

        let (cfg, needs_materialise) = super::bootstrap_from_store(&store)
            .await
            .expect("bootstrap");
        assert!(needs_materialise, "fresh install must request materialise");
        assert_eq!(
            cfg.hard_junk_threshold,
            EngineConfig::default().hard_junk_threshold
        );
        assert!(!cfg.shadow_mode);
    }

    /// Test 14: existing row — bootstrap returns the row's cfg with
    /// `needs_materialise = false`. Verifies the second-boot path.
    #[tokio::test]
    async fn bootstrap_from_store_reads_existing_row() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let store = Arc::new(SqliteStore::new("maild", conn).expect("store"));
        let mut router = PropsRouter::new("maild");

        let engine_lock = Arc::new(OnceLock::new());
        let runtime = register(&mut router, &store, engine_lock).expect("engine_config register");

        // Materialise a non-default row so we can tell bootstrap apart
        // from "fresh install returns defaults."
        let body = engine_config_to_value(&EngineConfig {
            shadow_mode: true,
            hard_junk_threshold: 18.0,
            ..EngineConfig::default()
        });
        runtime
            .set(key(), body, set_opts(Some(Version::zero())))
            .await
            .expect("seed row");

        let (cfg, needs_materialise) = super::bootstrap_from_store(&store)
            .await
            .expect("bootstrap");
        assert!(
            !needs_materialise,
            "existing row must not request materialise"
        );
        assert!(cfg.shadow_mode);
        assert_eq!(cfg.hard_junk_threshold, 18.0);
    }

    // The corrupted-row branch of `bootstrap_from_store` delegates to
    // `record_to_engine_config`, whose negative-path coverage lives in
    // the `record_read_rejects_*` tests above. Re-asserting it here
    // would duplicate that surface; the bootstrap-specific contract is
    // the NotFound→materialise branching (Tests 13/14).
}
