//! SPEC 12 property-substrate adoption for `maild.account_overrides`.
//!
//! Per-account overrides for the rules engine: disabled rule ids,
//! optional threshold override, and allow/block sender globs. Keyed
//! by **email** (mirrors `maild.accounts`); the sibling namespace's
//! `after_delete` cascades into this one and its `before_set` rejects
//! orphan re-adoption — both wired in `crate::props::accounts`. This
//! module owns the override namespace's schema, hooks, and storage
//! adapter only.
//!
//! ## Lifecycle
//!
//! `Simple` — no provisioning side-effects. `before_set` validates the
//! body and verifies the referenced account exists; on success the
//! substrate commits the value into `__props_values` via the bundled
//! [`JsonValuesMapping`].
//!
//! ## Cross-namespace dependency
//!
//! The override row depends on `maild.accounts.<email>`. The runtime
//! cycle is broken by an `Arc<OnceLock<Arc<Runtime>>>` populated after
//! both namespaces are registered (see `crate::main` startup); the
//! accounts side holds an `Arc<Runtime>` directly because its
//! registration happens *after* this module's `register` returns. See
//! the phase-2 plan doc (`src/_doc/planned/spec12-account-overrides-phase2.md`)
//! for the full rationale.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use cosmix_maild_rules::AccountOverrides;
use cosmix_maild_rules::glob_match::validate_sender_glob;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::record::RecordKey;
use cosmix_props::runtime::Runtime;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::store::StoreError;
use cosmix_props::value::PropValue;

/// Unqualified namespace name; fully qualified is `maild.account_overrides`.
pub const NAMESPACE: &str = "account_overrides";

/// Wire-error message prefix the CLI matches on to surface a recovery
/// hint when the substrate rejects an `accounts.props.set` because a
/// stale override row was found. Kept here (not in `accounts.rs`) so
/// the namespace owning the orphaned row also owns the prefix string.
pub const ORPHAN_OVERRIDE_PREFIX: &str = "orphan_override_detected:";

/// Message-body prefix the `before_set` hook emits when a `set` lands
/// for an email that has no `maild.accounts` row. Exported so the CLI
/// (and other Bus peers) can render an operator-recoverable hint
/// without substring-matching a literal.
pub const ACCOUNT_NOT_FOUND_PREFIX: &str = "account_not_found:";

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// SPEC 12 §4.3 schema. `email` doubles as the primary-key body field
/// per `Cardinality::Collection { primary_key_field: "email" }`; the
/// runtime enforces `value.email == wire-key` on Replace and on first
/// create with Patch.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "email".into(),
            ty: FieldType::Email,
            default: None,
            secret: false,
            help: "Account email this override applies to (primary key)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "disabled_rules".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Rule ids to skip for this account (unknown ids are a no-op)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "threshold_override".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::F64),
            },
            default: Some(PropValue::Null),
            secret: false,
            // EngineConfig::threshold default is 5.0 — raw rule
            // scores, not a 0..=1 probability. `before_set` rejects
            // NaN, infinities, and negatives; values above
            // hard_junk_threshold are permitted (operator intent to
            // disable the Continue→HardJunk transition).
            help: "Optional per-account engine threshold (raw rule score, finite, >= 0)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "allowlist_senders".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Sender globs whose mail bypasses spam routing".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "blocklist_senders".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Sender globs whose mail routes straight to Junk".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 §7.2 [`AuthPolicy`] — phase-2 grants every property
/// capability to every reachable peer (WireGuard `/24` is the trust
/// domain, same posture as `maild.accounts`). Phase 3+ narrows to
/// operator vs service tiers.
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.account_overrides",
            "props.write:maild.account_overrides",
            "props.describe:maild.account_overrides:public",
            "props.describe:maild.account_overrides:full",
            "props.audit:maild.account_overrides",
        ]
        .into_iter()
        .map(|s| Capability::new(s).expect("non-empty capability"))
        .collect()
    }
    AuthPolicy::new(resolve)
}

/// [`NamespaceSpec`] builder; takes the hooks parameter so tests can
/// substitute a fake handler. The storage adapter is the bundled
/// `__props_values` JSON mapping (Phase 2 has no sidecar SQL table).
pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "email".into(),
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

/// Hook handler for the `account_overrides` namespace.
///
/// `before_set` validates the body (email shape, threshold range, glob
/// compile, non-empty rule ids) and verifies that
/// `maild.accounts.<email>` exists. The accounts runtime is plumbed
/// via an `Arc<OnceLock<Arc<Runtime>>>` so registration order can be
/// `account_overrides` → `accounts` without a backwards-reaching
/// constructor parameter; the lock is populated *after* both
/// registrations return (see `crate::main`).
pub struct AccountOverridesHooks {
    /// Shared handle to the accounts runtime, populated by `main`
    /// after both namespaces are registered. `before_set` reads it on
    /// every invocation; if the lock is somehow empty at hook time
    /// (only possible if a request lands before `main` finishes wiring)
    /// the hook returns `hook_error` so the operator sees the misuse
    /// rather than a silent allow.
    accounts_runtime: Arc<OnceLock<Arc<Runtime>>>,
}

impl AccountOverridesHooks {
    pub fn new(accounts_runtime: Arc<OnceLock<Arc<Runtime>>>) -> Self {
        Self { accounts_runtime }
    }

    fn validate_email_shape(s: &str) -> Result<(), HookError> {
        // Mirror `accounts::AccountsHooks::validate_email_shape` —
        // duplicated rather than pub-cross-imported so the override
        // namespace's contract is self-contained.
        if s.is_empty() {
            return Err(HookError::validation("email is empty"));
        }
        if s.chars().any(char::is_whitespace) {
            return Err(HookError::validation("email contains whitespace"));
        }
        let mut parts = s.split('@');
        let local = parts.next().unwrap_or("");
        let domain = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(HookError::validation("email has multiple @ separators"));
        }
        if local.is_empty() || domain.is_empty() {
            return Err(HookError::validation(
                "email must have non-empty local and domain parts",
            ));
        }
        if !domain.contains('.') {
            return Err(HookError::validation("email domain must contain a dot"));
        }
        Ok(())
    }

    fn validate_body_shape(value: &PropValue) -> Result<(), HookError> {
        let obj = value
            .as_object()
            .ok_or_else(|| HookError::validation("account_overrides record must be an object"))?;
        if let Some(PropValue::String(s)) = obj.get("email") {
            Self::validate_email_shape(s)?;
        }
        match obj.get("threshold_override") {
            Some(PropValue::Float(f)) => {
                if !f.is_finite() || *f < 0.0 {
                    return Err(HookError::validation(format!(
                        "threshold_override must be finite and non-negative (got {f})"
                    )));
                }
                // The rules engine consumes `f32`; a finite f64 above
                // `f32::MAX` narrows to `f32::INFINITY` at read time
                // (see `record_to_overrides`). Reject at write time so
                // the stored shape is the same domain the engine
                // operates in — keeps the "validated row is always
                // engine-safe" invariant.
                if !(*f as f32).is_finite() {
                    return Err(HookError::validation(format!(
                        "threshold_override exceeds f32 range (got {f}; engine threshold is f32)"
                    )));
                }
            }
            Some(PropValue::Int(n)) => {
                if *n < 0 {
                    return Err(HookError::validation(format!(
                        "threshold_override must be non-negative (got {n})"
                    )));
                }
                // i64::MAX < f32::MAX (9.22e18 vs 3.40e38), so this
                // cast is always finite — precision loss only, which
                // the engine already accepts on its own `f32` config.
            }
            Some(PropValue::Null) | None => {}
            Some(other) => {
                return Err(HookError::validation(format!(
                    "threshold_override must be a number or null (got {})",
                    other.type_name()
                )));
            }
        }
        Self::validate_glob_list(obj.get("allowlist_senders"), "allowlist_senders")?;
        Self::validate_glob_list(obj.get("blocklist_senders"), "blocklist_senders")?;
        Self::validate_rule_id_list(obj.get("disabled_rules"))?;
        Ok(())
    }

    fn validate_glob_list(field: Option<&PropValue>, name: &str) -> Result<(), HookError> {
        let list = match field {
            // Field absent: schema default (empty list) applies. Explicit
            // `null` is not a List shape per the schema and must be
            // rejected so Bus callers can't persist a non-list value
            // that breaks downstream readers in the rules engine.
            None => return Ok(()),
            Some(PropValue::List(l)) => l,
            Some(other) => {
                return Err(HookError::validation(format!(
                    "{name} must be a list of strings (got {})",
                    other.type_name()
                )));
            }
        };
        for (i, entry) in list.iter().enumerate() {
            let pat = match entry {
                PropValue::String(s) => s.as_str(),
                other => {
                    return Err(HookError::validation(format!(
                        "{name}[{i}] must be a string (got {})",
                        other.type_name()
                    )));
                }
            };
            if let Err(msg) = validate_sender_glob(pat) {
                return Err(HookError::validation(format!(
                    "{name}[{i}] {pat:?} does not compile: {msg}"
                )));
            }
        }
        Ok(())
    }

    fn validate_rule_id_list(field: Option<&PropValue>) -> Result<(), HookError> {
        let list = match field {
            // Same posture as `validate_glob_list`: absent → schema
            // default (empty list); explicit `null` is rejected so a
            // non-list value can't slip into storage.
            None => return Ok(()),
            Some(PropValue::List(l)) => l,
            Some(other) => {
                return Err(HookError::validation(format!(
                    "disabled_rules must be a list of strings (got {})",
                    other.type_name()
                )));
            }
        };
        for (i, entry) in list.iter().enumerate() {
            match entry {
                PropValue::String(s) if !s.is_empty() => {}
                PropValue::String(_) => {
                    return Err(HookError::validation(format!(
                        "disabled_rules[{i}] must be a non-empty string"
                    )));
                }
                other => {
                    return Err(HookError::validation(format!(
                        "disabled_rules[{i}] must be a string (got {})",
                        other.type_name()
                    )));
                }
            }
        }
        Ok(())
    }
}

impl HookHandler for AccountOverridesHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Wire-key shape — caught here so a Patch that omits
            // `email` from the body still rejects malformed keys.
            Self::validate_email_shape(&ctx.key.key)?;

            // Body shape — type and range. The substrate's schema
            // walker enforces the structural types (lists are lists,
            // etc.); this hook adds semantic validation that the
            // schema cannot express.
            //
            // We validate BOTH `new` and (when present) `old`. `new`
            // alone is not sufficient: the substrate's `MergeMode::Patch`
            // shallow-merges the patch with the prior row (sqlite.rs
            // `merge_patch`), so an omitted list field in `new` would
            // preserve whatever shape the prior row had. Validating
            // `old` ensures the post-merge value is always schema-valid
            // regardless of merge mode — and forces any pre-existing
            // malformed row (e.g. one written before this check
            // landed) to be cleaned up via delete-then-create rather
            // than silently inherited by a Patch.
            if let Some(old) = &ctx.old {
                Self::validate_body_shape(old).map_err(|e| match e {
                    HookError::Validation { message } => HookError::Validation {
                        message: format!(
                            "prior row is malformed; delete-then-create to recover: {message}"
                        ),
                    },
                    other => other,
                })?;
            }
            if let Some(new) = &ctx.new {
                Self::validate_body_shape(new)?;
            }

            // Cross-namespace existence check. The accounts runtime
            // handle is populated post-registration; an empty lock
            // means the daemon is in an inconsistent startup state
            // and we MUST refuse rather than silently allow.
            let accounts_rt = self.accounts_runtime.get().ok_or_else(|| {
                HookError::hook(
                    "account_overrides hook fired before accounts runtime was wired \
                     (daemon-internal startup race)",
                )
            })?;
            let accounts_ns = crate::props::accounts::namespace_name();
            let accounts_key = RecordKey::collection(accounts_ns, ctx.key.key.clone());
            match accounts_rt.store().get(&accounts_key).await {
                Ok(_) => Ok(()),
                Err(StoreError::NotFound) => Err(HookError::validation(format!(
                    "{ACCOUNT_NOT_FOUND_PREFIX} no maild.accounts row for {:?}",
                    ctx.key.key
                ))),
                Err(e) => Err(HookError::hook(format!(
                    "account_overrides cross-namespace read failed: {e}"
                ))),
            }
        })
    }
}

/// Wire the `maild.account_overrides` namespace into the substrate.
/// Returns the namespace's `Arc<Runtime>` so the caller can hand it
/// to `accounts::register` (the cascade direction).
pub fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    accounts_runtime: Arc<OnceLock<Arc<Runtime>>>,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(AccountOverridesHooks::new(accounts_runtime));
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register account_overrides namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register account_overrides runtime on router: {e}"))?;
    Ok(runtime)
}

/// Convert a substrate record value into the rules-engine type. Caller
/// must have validated that the value came from this namespace (the
/// `before_set` hook is the only writer, and it enforces shape); this
/// function still returns `Err` rather than panicking on a malformed
/// row so a hand-edited / corrupted record surfaces as a delivery-path
/// warning instead of taking the SMTP session down.
///
/// Narrowing semantics:
/// * `threshold_override` `f64`→`f32`: precision loss is acceptable —
///   the engine's own `EngineConfig::threshold` is `f32`. The hook
///   already rejected NaN, infinities, and negatives, so the `as f32`
///   cast cannot introduce surprising values.
/// * Lists are walked once; entries that aren't `String` skip the
///   list (returning `Err`) rather than silently dropping items.
fn record_to_overrides(value: &PropValue) -> Result<AccountOverrides> {
    let obj = value.as_object().with_context(|| {
        format!(
            "account_overrides record is not an object (got {})",
            value.type_name()
        )
    })?;

    // Defensive belt — the hook is the gate, but a hand-edited or
    // corrupted row could violate the engine-safe domain. Mirror the
    // hook's full threshold checks (finite, non-negative, narrows
    // cleanly to f32) on the read path so a bad row surfaces as an
    // Err the delivery caller maps to defaults + warn-log, instead of
    // feeding `+inf` or a negative threshold to the rules engine.
    let threshold_override = match obj.get("threshold_override") {
        Some(PropValue::Float(f)) => {
            if !f.is_finite() || *f < 0.0 {
                anyhow::bail!(
                    "account_overrides.threshold_override {f} violates engine domain (corrupted row?)"
                );
            }
            let narrowed = *f as f32;
            if !narrowed.is_finite() {
                anyhow::bail!(
                    "account_overrides.threshold_override {f} overflows f32 (corrupted row?)"
                );
            }
            Some(narrowed)
        }
        Some(PropValue::Int(n)) => {
            if *n < 0 {
                anyhow::bail!(
                    "account_overrides.threshold_override {n} is negative (corrupted row?)"
                );
            }
            Some(*n as f32)
        }
        Some(PropValue::Null) | None => None,
        Some(other) => {
            anyhow::bail!(
                "account_overrides.threshold_override has unexpected type {}",
                other.type_name()
            );
        }
    };

    fn read_string_list(field: Option<&PropValue>, name: &str) -> Result<Vec<String>> {
        match field {
            None | Some(PropValue::Null) => Ok(Vec::new()),
            Some(PropValue::List(items)) => items
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    PropValue::String(s) => Ok(s.clone()),
                    other => Err(anyhow::anyhow!(
                        "account_overrides.{name}[{i}] is not a string (got {})",
                        other.type_name()
                    )),
                })
                .collect(),
            Some(other) => Err(anyhow::anyhow!(
                "account_overrides.{name} is not a list (got {})",
                other.type_name()
            )),
        }
    }

    Ok(AccountOverrides {
        disabled_rules: read_string_list(obj.get("disabled_rules"), "disabled_rules")?,
        threshold_override,
        allowlist_senders: read_string_list(obj.get("allowlist_senders"), "allowlist_senders")?,
        blocklist_senders: read_string_list(obj.get("blocklist_senders"), "blocklist_senders")?,
    })
}

/// Read a single account's overrides from the substrate.
///
/// * `Ok(default)` when the row is absent or tombstoned (covers the
///   pre-Phase-2 baseline: every account silently used defaults).
/// * `Ok(overrides)` on a successful read + conversion.
/// * `Err` on any other `StoreError` or a malformed row. The SMTP
///   delivery caller maps this back to `AccountOverrides::default()`
///   with a warn-log so operators see storage degradation instead of
///   getting silent permissive defaults.
pub async fn read_account_overrides_by_email(
    overrides_runtime: &Runtime,
    email: &str,
) -> Result<AccountOverrides> {
    Ok(resolve_account_overrides_by_email(overrides_runtime, email)
        .await?
        .overrides)
}

/// Same wire path as [`read_account_overrides_by_email`] but also
/// returns the substrate's row metadata for audit-stream correlation.
/// `version` and `nseq` are `None` iff the record was absent or
/// tombstoned (caller used schema defaults); callers that want to
/// surface "no override row exists" vs "row exists at version N"
/// distinctly should branch on that.
#[derive(Debug, Clone)]
pub struct ResolvedOverrides {
    pub overrides: AccountOverrides,
    pub version: Option<u64>,
    pub nseq: Option<u64>,
}

/// C4 explain-path resolver: identical defaulting + error-shape rules
/// as the simpler `read_account_overrides_by_email`, with the substrate
/// `Record.version` + `Snapshot.observed_nseq` returned alongside so
/// `maild.rules.explain` can render values an operator can correlate
/// against the `maild.account_overrides.audit` stream.
pub async fn resolve_account_overrides_by_email(
    overrides_runtime: &Runtime,
    email: &str,
) -> Result<ResolvedOverrides> {
    let key = RecordKey::collection(namespace_name(), email);
    match overrides_runtime.store().get(&key).await {
        Ok(snapshot) => {
            let version = snapshot.value.version.0;
            let nseq = snapshot.observed_nseq.0;
            let overrides = record_to_overrides(&snapshot.value.value)?;
            Ok(ResolvedOverrides {
                overrides,
                version: Some(version),
                nseq: Some(nseq),
            })
        }
        Err(StoreError::NotFound) => Ok(ResolvedOverrides {
            overrides: AccountOverrides::default(),
            version: None,
            nseq: None,
        }),
        Err(e) => Err(anyhow::anyhow!("account_overrides read for {email:?}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, Version};
    use cosmix_props::runtime::SetOpts;
    use cosmix_props::store::{MergeMode, PropertyStore};
    use std::collections::BTreeMap;

    fn key_for(email: &str) -> RecordKey {
        RecordKey::collection(namespace_name(), email)
    }

    fn value_obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PropValue::Object(m)
    }

    #[test]
    fn schema_lists_five_fields() {
        let s = schema();
        for f in [
            "email",
            "disabled_rules",
            "threshold_override",
            "allowlist_senders",
            "blocklist_senders",
        ] {
            assert!(s.field(f).is_some(), "field {f} present");
        }
    }

    #[test]
    fn spec_is_simple_collection_keyed_on_email() {
        let s = spec(Hooks::noop());
        assert!(!s.is_saga());
        match &s.cardinality {
            Cardinality::Collection { primary_key_field } => {
                assert_eq!(primary_key_field, "email");
            }
            other => panic!("unexpected cardinality: {other:?}"),
        }
        match &s.storage {
            StorageBackendKind::SqliteTable { table } => assert_eq!(table, "__props_values"),
            other => panic!("unexpected storage: {other:?}"),
        }
    }

    #[test]
    fn auth_policy_grants_phase2_caps() {
        let caps = auth_policy().resolve(&PeerIdentity::default());
        for c in [
            "props.read:maild.account_overrides",
            "props.write:maild.account_overrides",
            "props.describe:maild.account_overrides:public",
            "props.describe:maild.account_overrides:full",
            "props.audit:maild.account_overrides",
        ] {
            assert!(
                caps.contains(&Capability::new(c).expect("non-empty capability")),
                "cap {c} granted"
            );
        }
    }

    fn ctx_with(value: PropValue) -> HookCtx {
        HookCtx {
            key: key_for("user@example.com"),
            old: None,
            new: Some(value),
            version: Version::zero(),
            actor: Actor::service("test").expect("valid actor"),
            merge: Some(cosmix_props::store::MergeMode::Replace),
            origin: cosmix_props::WriteOrigin::caller(),
        }
    }

    #[tokio::test]
    async fn before_set_rejects_bad_email_wire_key() {
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let mut c = ctx_with(value_obj(&[]));
        c.key = RecordKey::collection(namespace_name(), "not-an-email");
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(matches!(err, HookError::Validation { .. }));
    }

    #[tokio::test]
    async fn before_set_rejects_nan_threshold() {
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let c = ctx_with(value_obj(&[(
            "threshold_override",
            PropValue::Float(f64::NAN),
        )]));
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(
            matches!(&err, HookError::Validation { message } if message.contains("threshold_override")),
            "expected threshold validation error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn before_set_rejects_threshold_above_f32_max() {
        // f64 value finite + non-negative but larger than `f32::MAX`
        // (3.4e38) would narrow to `f32::INFINITY` when the rules
        // engine reads it. C1's `f.is_finite() && f >= 0.0` passes; the
        // additional f32-range guard catches it before storage.
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let c = ctx_with(value_obj(&[(
            "threshold_override",
            PropValue::Float(1.0e40_f64),
        )]));
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(
            matches!(&err, HookError::Validation { message } if message.contains("exceeds f32 range")),
            "expected f32-range validation error, got {err:?}"
        );
    }

    #[test]
    fn record_to_overrides_rejects_negative_float_threshold() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Float(-1.0)),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = super::record_to_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("violates engine domain"),
            "expected engine-domain error, got {err}"
        );
    }

    #[test]
    fn record_to_overrides_rejects_negative_int_threshold() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Int(-3)),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = super::record_to_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("is negative"),
            "expected negative-int error, got {err}"
        );
    }

    #[test]
    fn record_to_overrides_rejects_nan_threshold() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Float(f64::NAN)),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = super::record_to_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("violates engine domain"),
            "expected engine-domain error, got {err}"
        );
    }

    #[test]
    fn record_to_overrides_rejects_f32_overflow_threshold() {
        // Defensive belt — a stored row that somehow snuck past the
        // hook (e.g. hand-edited SQL) must not feed +inf to the engine.
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Float(1.0e40_f64)),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = super::record_to_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("overflows f32"),
            "expected overflow error, got {err}"
        );
    }

    #[tokio::test]
    async fn before_set_rejects_negative_threshold() {
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let c = ctx_with(value_obj(&[("threshold_override", PropValue::Float(-0.5))]));
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(matches!(err, HookError::Validation { .. }));
    }

    #[tokio::test]
    async fn before_set_allows_threshold_above_hard_junk() {
        // A threshold of 999.0 is well above the engine's
        // hard_junk_threshold (15.0); the operator is deliberately
        // disabling the Continue→HardJunk transition. The hook must
        // allow this — and then fail downstream only because the
        // accounts runtime is empty (validation passes, cross-ns
        // check errors with hook_error in this minimal test rig).
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let c = ctx_with(value_obj(&[(
            "threshold_override",
            PropValue::Float(999.0),
        )]));
        let err = hooks.before_set(&c).await.unwrap_err();
        // Reaches the cross-ns step, which fails with hook_error
        // because the OnceLock is empty.
        assert!(
            matches!(&err, HookError::Hook { message } if message.contains("startup race")),
            "expected hook_error from empty lock, got {err:?}"
        );
    }

    #[tokio::test]
    async fn before_set_accepts_legal_globs() {
        // The glob compiler escapes all non-alphanumeric input so almost
        // any string is a "valid" glob — the practical contract is that
        // validator and runtime share `SenderGlobMatcher::compile`, so a
        // pattern accepted here cannot fail at delivery time. Assert
        // that representative shapes (literal, wildcard, special-char
        // literal) all pass body-shape validation. We still hit the
        // empty-OnceLock hook_error on the cross-ns step.
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        for pat in ["brother@example.com", "*@vendor.example", "[brackets]@x.y"] {
            let c = ctx_with(value_obj(&[(
                "allowlist_senders",
                PropValue::List(vec![PropValue::String(pat.to_string())]),
            )]));
            let err = hooks.before_set(&c).await.unwrap_err();
            assert!(
                matches!(&err, HookError::Hook { message } if message.contains("startup race")),
                "pattern {pat:?} should pass shape validation, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn before_set_rejects_malformed_prior_row() {
        // A Patch write whose body is fine but whose `ctx.old` is
        // malformed (Null in a list field) would silently preserve the
        // bad shape under `merge_patch`. The hook validates `old` too,
        // so the operation is refused — operator recovers via
        // delete-then-create.
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let bad_old = value_obj(&[
            ("email", PropValue::String("user@example.com".into())),
            ("disabled_rules", PropValue::Null),
            ("threshold_override", PropValue::Null),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let mut c = ctx_with(value_obj(&[("threshold_override", PropValue::Float(7.5))]));
        c.old = Some(bad_old);
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(
            matches!(&err, HookError::Validation { message }
                if message.contains("prior row is malformed")
                    && message.contains("disabled_rules")),
            "expected malformed-prior validation error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn before_set_rejects_null_for_list_fields() {
        // Schema declares disabled_rules / allowlist_senders /
        // blocklist_senders as `FieldType::List<String>` (not
        // `Option<List>`); a wire `null` is not a List shape and must
        // be rejected as validation_error so downstream rule-engine
        // readers can rely on the type.
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        for field in ["disabled_rules", "allowlist_senders", "blocklist_senders"] {
            let c = ctx_with(value_obj(&[(field, PropValue::Null)]));
            let err = hooks.before_set(&c).await.unwrap_err();
            assert!(
                matches!(&err, HookError::Validation { message } if message.contains(field)),
                "field {field}: expected validation_error mentioning the field, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn before_set_rejects_empty_rule_id() {
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        let c = ctx_with(value_obj(&[(
            "disabled_rules",
            PropValue::List(vec![PropValue::String(String::new())]),
        )]));
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(matches!(err, HookError::Validation { .. }));
    }

    #[tokio::test]
    async fn before_set_rejects_when_account_runtime_unwired() {
        let hooks = AccountOverridesHooks::new(Arc::new(OnceLock::new()));
        // Body passes shape checks; cross-ns step hits the empty lock.
        let c = ctx_with(value_obj(&[]));
        let err = hooks.before_set(&c).await.unwrap_err();
        assert!(
            matches!(&err, HookError::Hook { message } if message.contains("startup race")),
            "expected startup-race hook_error, got {err:?}"
        );
    }

    /// End-to-end: register the namespace, wire the lock, and verify a
    /// `set` against an unknown account is rejected as
    /// `account_not_found:` while a `set` against a known account
    /// (we register the accounts namespace too and seed the row)
    /// succeeds. Round-trips JSON list/float/null fields.
    #[tokio::test]
    async fn end_to_end_round_trip_through_runtime() {
        use crate::props::accounts;
        use cosmix_props::sqlite::SqliteStore;
        use rusqlite::Connection;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("maild.db");
        let mds_dir = tmp.path().join("mds");
        std::fs::create_dir_all(&mds_dir).unwrap();

        // Maild's connection — runs the application schema (accounts
        // table for the seed below).
        let app_conn = Connection::open(&db_path).unwrap();
        app_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        app_conn.execute_batch(crate::db::SCHEMA).unwrap();
        let db_conn = Arc::new(std::sync::Mutex::new(app_conn));

        // Substrate connection.
        let props_conn = Connection::open(&db_path).unwrap();
        props_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        let store = Arc::new(SqliteStore::new("maild", props_conn).unwrap());

        let mds = Arc::new(cosmix_mds::SqliteCasMds::open(mds_dir.to_str().unwrap()).unwrap());
        let mailstore = Arc::new(crate::mailstore::SqliteMailStore::new(mds));

        let mut router = PropsRouter::new("maild");
        let accounts_lock = Arc::new(OnceLock::new());
        let overrides_rt = register(&mut router, &store, accounts_lock.clone()).unwrap();
        let accounts_rt = accounts::register(
            &mut router,
            &store,
            mailstore.clone(),
            db_conn.clone(),
            overrides_rt.clone(),
            Arc::new(OnceLock::new()),
        )
        .unwrap();
        accounts_lock
            .set(accounts_rt.clone())
            .map_err(|_| ())
            .expect("accounts_lock empty");

        // (1) Unknown account → `account_not_found:` validation_error.
        let opts = SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::operator("test").expect("valid actor"),
            cause: None,
            ts_ms: 0,
        };
        let val = value_obj(&[
            ("email", PropValue::String("nope@example.com".into())),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("threshold_override", PropValue::Null),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = overrides_rt
            .set(key_for("nope@example.com"), val.clone(), opts.clone())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("account_not_found:"),
            "expected account_not_found, got {msg:?}"
        );

        // (2) Seed an account, then overrides succeed and round-trip.
        let acct_val = value_obj(&[
            ("email", PropValue::String("ok@example.com".into())),
            (
                "password",
                PropValue::String("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
            ),
            ("name", PropValue::String("Ok".into())),
            ("quota", PropValue::Int(0)),
            ("spam_enabled", PropValue::Bool(true)),
            ("spam_threshold", PropValue::Float(0.5)),
        ]);
        accounts_rt
            .set(
                RecordKey::collection(accounts::namespace_name(), "ok@example.com"),
                acct_val,
                opts.clone(),
            )
            .await
            .expect("account seed succeeds");

        let ov_val = value_obj(&[
            ("email", PropValue::String("ok@example.com".into())),
            (
                "disabled_rules",
                PropValue::List(vec![PropValue::String("R-001".into())]),
            ),
            ("threshold_override", PropValue::Float(7.5)),
            (
                "allowlist_senders",
                PropValue::List(vec![PropValue::String("*@vendor.example".into())]),
            ),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let outcome = overrides_rt
            .set(key_for("ok@example.com"), ov_val.clone(), opts.clone())
            .await
            .expect("override set succeeds");
        assert!(
            outcome.complete_event.is_none(),
            "Simple lifecycle: no Complete"
        );

        // Round-trip through the store.
        let read = store
            .get(&key_for("ok@example.com"))
            .await
            .expect("override row reads back");
        assert_eq!(read.value.value, ov_val);
    }

    #[test]
    fn record_to_overrides_round_trips_canonical_shape() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            (
                "disabled_rules",
                PropValue::List(vec![
                    PropValue::String("R-001".into()),
                    PropValue::String("R-042".into()),
                ]),
            ),
            ("threshold_override", PropValue::Float(7.5)),
            (
                "allowlist_senders",
                PropValue::List(vec![PropValue::String("*@vendor.example".into())]),
            ),
            (
                "blocklist_senders",
                PropValue::List(vec![PropValue::String("spammer@*.bad".into())]),
            ),
        ]);
        let ov = super::record_to_overrides(&v).unwrap();
        assert_eq!(ov.disabled_rules, vec!["R-001".to_string(), "R-042".into()]);
        assert!((ov.threshold_override.unwrap() - 7.5_f32).abs() < f32::EPSILON);
        assert_eq!(ov.allowlist_senders, vec!["*@vendor.example".to_string()]);
        assert_eq!(ov.blocklist_senders, vec!["spammer@*.bad".to_string()]);
    }

    #[test]
    fn record_to_overrides_null_threshold_yields_none() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Null),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let ov = super::record_to_overrides(&v).unwrap();
        assert!(ov.threshold_override.is_none());
    }

    #[test]
    fn record_to_overrides_int_threshold_narrows_to_f32() {
        // JSON `7` round-trips as Int, not Float. The hook accepts both
        // shapes, so the converter must too.
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("threshold_override", PropValue::Int(7)),
            ("disabled_rules", PropValue::List(Vec::new())),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let ov = super::record_to_overrides(&v).unwrap();
        assert_eq!(ov.threshold_override, Some(7.0_f32));
    }

    #[test]
    fn record_to_overrides_rejects_non_string_list_entry() {
        let v = value_obj(&[
            ("email", PropValue::String("u@example.com".into())),
            ("disabled_rules", PropValue::List(vec![PropValue::Int(42)])),
            ("threshold_override", PropValue::Null),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ]);
        let err = super::record_to_overrides(&v).unwrap_err();
        assert!(
            err.to_string().contains("disabled_rules[0]"),
            "expected disabled_rules error, got {err}"
        );
    }

    #[tokio::test]
    async fn read_account_overrides_by_email_returns_default_when_absent() {
        let rig = build_rig().await;
        // No account, no override row — helper short-circuits NotFound
        // into the default `AccountOverrides`.
        let ov = super::read_account_overrides_by_email(&rig.overrides_rt, "missing@example.com")
            .await
            .expect("absent row reads as default");
        assert!(ov.disabled_rules.is_empty());
        assert!(ov.threshold_override.is_none());
        assert!(ov.allowlist_senders.is_empty());
        assert!(ov.blocklist_senders.is_empty());
    }

    #[tokio::test]
    async fn read_account_overrides_by_email_round_trips_set_value() {
        let rig = build_rig().await;
        let email = "ovread@example.com";
        rig.accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .expect("account seed succeeds");
        rig.overrides_rt
            .set(key_for(email), seed_overrides_value(email), opts_now())
            .await
            .expect("overrides set succeeds");

        let ov = super::read_account_overrides_by_email(&rig.overrides_rt, email)
            .await
            .expect("override reads back");
        assert_eq!(ov.disabled_rules, vec!["R-001".to_string()]);
        assert!((ov.threshold_override.unwrap() - 7.5_f32).abs() < f32::EPSILON);
    }

    /// Shared fixture for the cascade / orphan integration tests. Wires
    /// account_overrides and accounts against a single SQLite file the
    /// way `main.rs` does — overrides first, accounts second, lock
    /// populated last — and returns both runtimes plus the raw maild
    /// connection (for direct-SQL orphan staging).
    struct Phase2Rig {
        _tmp: tempfile::TempDir,
        store: Arc<cosmix_props::sqlite::SqliteStore>,
        overrides_rt: Arc<Runtime>,
        accounts_rt: Arc<Runtime>,
        db_conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    }

    async fn build_rig() -> Phase2Rig {
        use crate::props::accounts;
        use cosmix_props::sqlite::SqliteStore;
        use rusqlite::Connection;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("maild.db");
        let mds_dir = tmp.path().join("mds");
        std::fs::create_dir_all(&mds_dir).unwrap();

        let app_conn = Connection::open(&db_path).unwrap();
        app_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        app_conn.execute_batch(crate::db::SCHEMA).unwrap();
        let db_conn = Arc::new(std::sync::Mutex::new(app_conn));

        let props_conn = Connection::open(&db_path).unwrap();
        props_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )
            .unwrap();
        let store = Arc::new(SqliteStore::new("maild", props_conn).unwrap());

        let mds = Arc::new(cosmix_mds::SqliteCasMds::open(mds_dir.to_str().unwrap()).unwrap());
        let mailstore = Arc::new(crate::mailstore::SqliteMailStore::new(mds));

        let mut router = PropsRouter::new("maild");
        let accounts_lock = Arc::new(OnceLock::new());
        let overrides_rt = register(&mut router, &store, accounts_lock.clone()).unwrap();
        let accounts_rt = accounts::register(
            &mut router,
            &store,
            mailstore,
            db_conn.clone(),
            overrides_rt.clone(),
            Arc::new(OnceLock::new()),
        )
        .unwrap();
        accounts_lock
            .set(accounts_rt.clone())
            .map_err(|_| ())
            .expect("accounts_lock empty");

        Phase2Rig {
            _tmp: tmp,
            store,
            overrides_rt,
            accounts_rt,
            db_conn,
        }
    }

    fn opts_now() -> SetOpts {
        SetOpts {
            expected_version: None,
            merge: MergeMode::Replace,
            actor: Actor::operator("test").expect("valid actor"),
            cause: None,
            ts_ms: 0,
        }
    }

    fn seed_account_value(email: &str) -> PropValue {
        value_obj(&[
            ("email", PropValue::String(email.into())),
            (
                "password",
                PropValue::String("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
            ),
            ("name", PropValue::String("Test".into())),
            ("quota", PropValue::Int(0)),
            ("spam_enabled", PropValue::Bool(true)),
            ("spam_threshold", PropValue::Float(0.5)),
        ])
    }

    fn seed_overrides_value(email: &str) -> PropValue {
        value_obj(&[
            ("email", PropValue::String(email.into())),
            (
                "disabled_rules",
                PropValue::List(vec![PropValue::String("R-001".into())]),
            ),
            ("threshold_override", PropValue::Float(7.5)),
            ("allowlist_senders", PropValue::List(Vec::new())),
            ("blocklist_senders", PropValue::List(Vec::new())),
        ])
    }

    /// Cascade happy path: deleting an account through the runtime
    /// fires `accounts.after_delete`, which removes the matching
    /// override row. After the delete the overrides store reports
    /// `NotFound` for the same key.
    #[tokio::test]
    async fn cascade_deletes_overrides_row_on_account_delete() {
        use cosmix_props::record::Actor as RecordActor;
        use cosmix_props::runtime::DeleteOpts;
        use cosmix_props::store::StoreError;

        let rig = build_rig().await;
        let email = "cascade@example.com";

        rig.accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .expect("account seed succeeds");
        rig.overrides_rt
            .set(key_for(email), seed_overrides_value(email), opts_now())
            .await
            .expect("overrides set succeeds");
        // Pre-condition: overrides row visible.
        rig.store
            .get(&key_for(email))
            .await
            .expect("overrides row exists before delete");

        let del_opts = DeleteOpts {
            expected_version: None,
            actor: RecordActor::operator("test").expect("valid actor"),
            cause: Some("test:cascade".into()),
            ts_ms: 0,
        };
        rig.accounts_rt
            .delete(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                del_opts,
            )
            .await
            .expect("account delete succeeds");

        let post = rig.store.get(&key_for(email)).await;
        assert!(
            matches!(post, Err(StoreError::NotFound)),
            "override row should be gone after cascade, got {post:?}"
        );
    }

    /// Cascade failure path: if the cascade-delete itself errors (not
    /// the `NotFound` happy case but a real storage failure), the
    /// parent account.delete must still succeed and the override row
    /// must survive — accounts.after_delete swallows the error,
    /// emits the operator-recovery hint, and the orphan-detection
    /// check is what catches the next account.create. We synthesise a
    /// storage failure by deleting the overrides namespace's
    /// `__props_meta` row mid-flow, which makes `Runtime::delete`
    /// fail with `StoreError::Storage("namespace ... not registered")`.
    #[tokio::test]
    async fn cascade_failure_keeps_account_delete_ok_and_leaves_orphan() {
        use cosmix_props::record::Actor as RecordActor;
        use cosmix_props::runtime::DeleteOpts;

        let rig = build_rig().await;
        let email = "cf@example.com";

        rig.accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .expect("account seed succeeds");
        rig.overrides_rt
            .set(key_for(email), seed_overrides_value(email), opts_now())
            .await
            .expect("overrides set succeeds");

        // Synthetic storage failure for the cascade: strip the
        // overrides namespace cursor row. Substrate `get` requires it
        // (sqlite.rs:562) and surfaces `StoreError::Storage`, which
        // accounts.after_delete must swallow.
        {
            let conn = rig.db_conn.lock().unwrap();
            conn.execute(
                "DELETE FROM __props_meta WHERE namespace = 'account_overrides'",
                [],
            )
            .unwrap();
        }

        let del_opts = DeleteOpts {
            expected_version: None,
            actor: RecordActor::operator("test").expect("valid actor"),
            cause: Some("test:cascade-failure".into()),
            ts_ms: 0,
        };
        rig.accounts_rt
            .delete(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                del_opts,
            )
            .await
            .expect("account delete must succeed even when cascade fails");

        // Override row still present in storage (the cascade aborted
        // before commit_delete). Read raw — the substrate path is
        // intentionally broken here.
        let conn = rig.db_conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM __props_values \
                 WHERE namespace = 'account_overrides' AND key = ?1",
                rusqlite::params![email],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "override row must survive cascade failure (operator cleans up)"
        );
    }

    /// Orphan adoption: simulate the (rare) case where the cascade
    /// failed silently and left an override row behind, then re-create
    /// the parent account. The accounts.before_set create branch must
    /// detect the orphan and reject with `orphan_override_detected:`.
    ///
    /// We synthesise the "cascade failed" state by deleting the parent
    /// account rows directly via SQL (bypassing `Runtime::delete` so
    /// `after_delete` doesn't fire), leaving the override row intact.
    #[tokio::test]
    async fn orphan_override_rejects_account_re_create() {
        use crate::props::account_overrides::ORPHAN_OVERRIDE_PREFIX;
        let rig = build_rig().await;
        let email = "orphan@example.com";

        rig.accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .expect("account seed succeeds");
        rig.overrides_rt
            .set(key_for(email), seed_overrides_value(email), opts_now())
            .await
            .expect("overrides set succeeds");

        // Bypass the cascade: strip the parent account at the SQL layer
        // (substrate metadata + maild's `accounts` row). The override
        // row in `__props_values` survives.
        {
            let conn = rig.db_conn.lock().unwrap();
            conn.execute(
                "DELETE FROM __props_records WHERE namespace = 'accounts' AND key = ?1",
                rusqlite::params![email],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM accounts WHERE email = ?1",
                rusqlite::params![email],
            )
            .unwrap();
        }
        // Pre-condition for the orphan check: override still readable.
        rig.store
            .get(&key_for(email))
            .await
            .expect("override row survives the simulated cascade failure");

        let err = rig
            .accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(ORPHAN_OVERRIDE_PREFIX),
            "expected orphan_override_detected, got {msg:?}"
        );
    }

    /// TOCTOU between accounts.delete and overrides.set on the same
    /// email. Substrate serialization is per-namespace, so the two
    /// runtimes can interleave. The contract is not "this race never
    /// happens" but "whatever the race outcome, the system is
    /// recoverable":
    ///   - the parent account ends up gone (delete returns Ok),
    ///   - the override is either also gone (clean), or it survives
    ///     and the next account.set for the same email is rejected
    ///     with `orphan_override_detected:` (safety net catches it).
    #[tokio::test]
    async fn toctou_delete_vs_set_leaves_system_recoverable() {
        use crate::props::account_overrides::ORPHAN_OVERRIDE_PREFIX;
        use cosmix_props::record::Actor as RecordActor;
        use cosmix_props::runtime::DeleteOpts;
        use cosmix_props::store::StoreError;

        let rig = build_rig().await;
        let email = "race@example.com";

        rig.accounts_rt
            .set(
                RecordKey::collection(crate::props::accounts::namespace_name(), email),
                seed_account_value(email),
                opts_now(),
            )
            .await
            .expect("account seed succeeds");
        rig.overrides_rt
            .set(key_for(email), seed_overrides_value(email), opts_now())
            .await
            .expect("overrides seed succeeds");

        // Concurrent delete + set on the same email. Both outcomes are
        // legal — the runtime serialization picks an order — but the
        // post-state must be one of the two recoverable shapes.
        let acct_key = RecordKey::collection(crate::props::accounts::namespace_name(), email);
        let del_opts = DeleteOpts {
            expected_version: None,
            actor: RecordActor::operator("test").expect("valid actor"),
            cause: Some("test:toctou".into()),
            ts_ms: 0,
        };
        let updated_override = {
            let mut v = match seed_overrides_value(email) {
                PropValue::Object(m) => m,
                _ => unreachable!(),
            };
            v.insert("threshold_override".into(), PropValue::Float(9.9));
            PropValue::Object(v)
        };
        let (del_res, set_res) = tokio::join!(
            rig.accounts_rt.delete(acct_key.clone(), del_opts),
            rig.overrides_rt
                .set(key_for(email), updated_override, opts_now()),
        );
        // Delete is always Ok (cascade swallows even if it raced).
        del_res.expect("account delete must succeed regardless of race ordering");
        // Set is either Ok (won the race, override now an orphan) or
        // Err with `account_not_found:` (lost the race, before_set
        // saw the parent gone).
        let set_observed_account = match &set_res {
            Ok(_) => true,
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.contains("account_not_found:"),
                    "overrides.set should fail only with account_not_found, got {s:?}"
                );
                false
            }
        };

        // Account is gone either way.
        let post_acct = rig.store.get(&acct_key).await;
        assert!(
            matches!(post_acct, Err(StoreError::NotFound)),
            "account must be gone after race, got {post_acct:?}"
        );

        // Recovery: a fresh account.set with the same email is either
        // (a) accepted cleanly if no orphan survived, or (b) rejected
        // with orphan_override_detected if one did. Whichever happened,
        // run a recovery cycle and assert the system reaches a clean
        // state.
        let re_create = rig
            .accounts_rt
            .set(acct_key.clone(), seed_account_value(email), opts_now())
            .await;
        match re_create {
            Ok(_) => {
                // No orphan; that means either the cascade beat the
                // set, or the set lost the cross-ns check.
                assert!(
                    !set_observed_account
                        || matches!(
                            rig.store.get(&key_for(email)).await,
                            Err(StoreError::NotFound)
                        ),
                    "if account.set succeeded, no override row should be observable"
                );
            }
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.contains(ORPHAN_OVERRIDE_PREFIX),
                    "account re-create after race should only fail with orphan_override_detected, got {s:?}"
                );
                // Operator-recovery shape: clear the orphan via
                // overrides.delete, then re-create succeeds.
                rig.overrides_rt
                    .delete(
                        key_for(email),
                        DeleteOpts {
                            expected_version: None,
                            actor: RecordActor::operator("test").expect("valid actor"),
                            cause: Some("test:toctou-cleanup".into()),
                            ts_ms: 0,
                        },
                    )
                    .await
                    .expect("operator clears orphan");
                rig.accounts_rt
                    .set(acct_key, seed_account_value(email), opts_now())
                    .await
                    .expect("account.set succeeds after orphan cleanup");
            }
        }
    }
}
