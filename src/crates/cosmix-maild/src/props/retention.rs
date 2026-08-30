//! SPEC 12 property-substrate namespace for `maild.retention`.
//!
//! Singleton-cardinality namespace carrying the global mail-retention
//! policy: the per-folder age windows (Junk / Trash), the sweep cadence,
//! the runaway delete cap, and the dry-run kill-switch. This is the
//! config surface for the in-process retention worker (Phase 1) that
//! trims aged Junk/Trash memberships — the dovecot `expunge … savedbefore
//! Nd` analog.
//!
//! ## Safety posture (this auto-DELETES user mail)
//!
//! The defaults are **fully inert**: `junk_retention_days = 0` and
//! `trash_retention_days = 0` (0 = disabled), and `dry_run = true`. So
//! merely shipping the namespace + worker changes nothing until an
//! operator deliberately sets a non-zero window AND clears dry-run. The
//! retention clock keys on the per-membership `added_at` (when the
//! message entered that folder), matching dovecot `savedbefore` — NOT
//! the item's original receipt date — so a recently-trashed old email
//! gets the full window, not instant deletion.
//!
//! ## Cardinality / lifecycle
//!
//! `Singleton { canonical_key: "current" }`, `Simple` lifecycle. Unlike
//! `engine_config`, this namespace is NOT `require_version` (it carries
//! no cross-field invariant) and its row is **deletable** — deleting it
//! resets the worker to the inert defaults (a safe "stop retention"
//! gesture), so `before_delete` is left at the default allow.
//!
//! ## Source of truth
//!
//! The substrate row IS the retention policy; there is no TOML mirror
//! (same single-source decision as `engine_config` / `account_overrides`).
//! A sparse or absent row reads as [`RetentionConfig::default`].

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
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

/// Unqualified namespace name; fully qualified is `maild.retention`.
pub const NAMESPACE: &str = "retention";

/// Wire key the substrate fills in on responses (singleton).
pub const CANONICAL_KEY: &str = "current";

// Field bounds. `*_retention_days` of 0 means "disabled"; the ceiling is
// a sanity clamp (≈100 years) so a fat-fingered value can't underflow the
// cutoff arithmetic. `tick_minutes` floors at 1 (a 0 would busy-loop the
// worker) and caps at one week. `max_deletes_per_sweep` floors at 1.
const MAX_RETENTION_DAYS: u64 = 36_500;
const MIN_TICK_MINUTES: u64 = 1;
const MAX_TICK_MINUTES: u64 = 10_080;
const MIN_DELETES_PER_SWEEP: u64 = 1;
const MAX_DELETES_PER_SWEEP: u64 = 1_000_000;

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Effective retention policy read from the substrate row. A multiplier
/// of `*_retention_days = 0` disables that folder's sweep entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    pub junk_retention_days: u64,
    pub trash_retention_days: u64,
    pub tick_minutes: u64,
    pub max_deletes_per_sweep: u64,
    pub dry_run: bool,
    /// Explicit per-account opt-in list (account email addresses). The
    /// worker sweeps ONLY accounts named here — there is **no fleet
    /// auto-default**: an empty list means retention touches no account
    /// even when the windows are non-zero and dry_run is off. An operator
    /// arms each account by adding its email here (LOCKED decision: "opts
    /// each account in"). Per-account *windows* remain future work; today
    /// every armed account shares the global window.
    pub armed_accounts: Vec<String>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        // Fully inert: nothing is deleted until an operator arms it.
        Self {
            junk_retention_days: 0,
            trash_retention_days: 0,
            tick_minutes: 60,
            max_deletes_per_sweep: 5000,
            dry_run: true,
            armed_accounts: Vec::new(),
        }
    }
}

impl RetentionConfig {
    /// True when the worker can skip the whole sweep (no set enumeration,
    /// no tx): either no folder has a non-zero window, OR no account is
    /// opted in. Both gates must be open for any deletion to occur.
    pub fn is_disabled(&self) -> bool {
        (self.junk_retention_days == 0 && self.trash_retention_days == 0)
            || self.armed_accounts.is_empty()
    }
}

/// SPEC 12 §4.3 schema. Defaults mirror [`RetentionConfig::default`].
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "junk_retention_days".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(0)),
            secret: false,
            help: "Delete a Junk membership older than N days since it entered Junk (0 = disabled)"
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "trash_retention_days".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(0)),
            secret: false,
            help:
                "Delete a Trash membership older than N days since it entered Trash (0 = disabled)"
                    .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "tick_minutes".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(60)),
            secret: false,
            help: "Sweep cadence in minutes (1..=10080)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "max_deletes_per_sweep".into(),
            ty: FieldType::U64,
            default: Some(PropValue::UInt(5000)),
            secret: false,
            help:
                "Per-account runaway guard: at most N membership removals per sweep (1..=1_000_000)"
                    .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dry_run".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "When true, log what WOULD be removed and delete nothing (ships true)".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "armed_accounts".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Account emails the worker may sweep. EMPTY = no account swept (no fleet \
                   auto-default); add an email to opt that account in."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 §7.2 [`AuthPolicy`] — **operator-tier divergence** (mirrors
/// `webd.listeners`): read / describe / audit go to every WG peer, but
/// `props.write:maild.retention` — the cap that *arms* automatic mail
/// deletion (flips a window non-zero or clears `dry_run`) — is granted
/// only to a peer whose `service_name` (the Bus sender, `cmd.from`) is in
/// `operators`. **Empty `operators` ⇒ no remote arming at all** (the
/// shipped default), so retention cannot be turned on over Bus until an
/// operator is explicitly named in maild config. Daemon backend-origin
/// writes don't flow through `AuthPolicy` and are unaffected.
pub fn auth_policy(operators: Vec<String>) -> AuthPolicy {
    let read = Capability::from("props.read:maild.retention");
    let describe_public = Capability::from("props.describe:maild.retention:public");
    let describe_full = Capability::from("props.describe:maild.retention:full");
    let audit = Capability::from("props.audit:maild.retention");
    let write = Capability::from("props.write:maild.retention");
    AuthPolicy::new(move |peer: &PeerIdentity| {
        let mut caps = vec![
            read.clone(),
            describe_public.clone(),
            describe_full.clone(),
            audit.clone(),
        ];
        let is_operator = peer
            .service_name
            .as_deref()
            .is_some_and(|s| operators.iter().any(|o| o == s));
        if is_operator {
            caps.push(write.clone());
        }
        caps.into_iter().collect::<CapabilitySet>()
    })
}

pub fn spec(hooks: Hooks, operators: Vec<String>) -> NamespaceSpec {
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
    s.lifecycle = NamespaceLifecycle::Simple;
    s.auth = auth_policy(operators);
    s.hooks = hooks;
    s
}

/// Validation hook: bounds-checks the fields present in the request body.
/// No cross-field invariant and no `require_version`, so validating the
/// present fields (Patch carries only the changed ones; each prior write
/// was itself validated) is sufficient.
pub struct RetentionHooks;

impl HookHandler for RetentionHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let new = ctx
                .new
                .clone()
                .ok_or_else(|| HookError::hook("retention before_set fired with new=None"))?;
            let obj = new.as_object().ok_or_else(|| {
                HookError::validation(format!(
                    "retention record must be an object (got {})",
                    new.type_name()
                ))
            })?;
            // Only fields actually present are checked — a Patch that
            // omits a field leaves the prior (already-validated) value.
            check_u64_if_present(obj, "junk_retention_days", 0, MAX_RETENTION_DAYS)?;
            check_u64_if_present(obj, "trash_retention_days", 0, MAX_RETENTION_DAYS)?;
            check_u64_if_present(obj, "tick_minutes", MIN_TICK_MINUTES, MAX_TICK_MINUTES)?;
            check_u64_if_present(
                obj,
                "max_deletes_per_sweep",
                MIN_DELETES_PER_SWEEP,
                MAX_DELETES_PER_SWEEP,
            )?;
            check_bool_if_present(obj, "dry_run")?;
            check_string_list_if_present(obj, "armed_accounts")?;
            Ok(())
        })
    }
}

/// Wire the `maild.retention` namespace into the substrate. `operators`
/// is the write-allowlist (Bus sender names) — empty disables remote
/// arming entirely (see [`auth_policy`]).
pub fn register(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
    operators: Vec<String>,
) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(RetentionHooks);
    let spec = spec(hooks, operators);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register retention namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register retention runtime on router: {e}"))?;
    Ok(runtime)
}

/// Read the effective [`RetentionConfig`] from the substrate. An absent
/// row reads as the inert defaults; a present row reads each field
/// (missing fields fall back to the default), bound-clamped defensively
/// so a corrupted row cannot feed an out-of-range value into the
/// destructive worker.
pub async fn read_config(runtime: &Runtime) -> Result<RetentionConfig> {
    let key = RecordKey::singleton(namespace_name());
    match runtime.store().get(&key).await {
        Ok(snap) => record_to_config(&snap.value.value)
            .context("retention: existing substrate row is invalid"),
        Err(StoreError::NotFound) => Ok(RetentionConfig::default()),
        Err(e) => Err(anyhow::anyhow!("retention: read substrate row: {e}")),
    }
}

/// Project a substrate `PropValue::Object` into a [`RetentionConfig`],
/// supplying defaults for absent fields and bound-clamping each numeric.
pub fn record_to_config(value: &PropValue) -> Result<RetentionConfig> {
    let d = RetentionConfig::default();
    let obj = value.as_object().with_context(|| {
        format!(
            "retention record is not an object (got {})",
            value.type_name()
        )
    })?;
    Ok(RetentionConfig {
        junk_retention_days: read_u64_or(
            obj,
            "junk_retention_days",
            0,
            MAX_RETENTION_DAYS,
            d.junk_retention_days,
        )?,
        trash_retention_days: read_u64_or(
            obj,
            "trash_retention_days",
            0,
            MAX_RETENTION_DAYS,
            d.trash_retention_days,
        )?,
        tick_minutes: read_u64_or(
            obj,
            "tick_minutes",
            MIN_TICK_MINUTES,
            MAX_TICK_MINUTES,
            d.tick_minutes,
        )?,
        max_deletes_per_sweep: read_u64_or(
            obj,
            "max_deletes_per_sweep",
            MIN_DELETES_PER_SWEEP,
            MAX_DELETES_PER_SWEEP,
            d.max_deletes_per_sweep,
        )?,
        dry_run: read_bool_or(obj, "dry_run", d.dry_run)?,
        armed_accounts: read_string_list_or(obj, "armed_accounts", d.armed_accounts)?,
    })
}

// ---- field helpers ----

fn check_u64_if_present(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    min: u64,
    max: u64,
) -> Result<(), HookError> {
    let v = match obj.get(field) {
        None => return Ok(()),
        Some(PropValue::UInt(n)) => *n,
        Some(PropValue::Int(n)) if *n >= 0 => *n as u64,
        Some(PropValue::Int(n)) => {
            return Err(HookError::validation(format!(
                "{field} {n} must be non-negative"
            )));
        }
        Some(other) => {
            return Err(HookError::validation(format!(
                "{field} must be an unsigned integer (got {})",
                other.type_name()
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

fn check_bool_if_present(obj: &BTreeMap<String, PropValue>, field: &str) -> Result<(), HookError> {
    match obj.get(field) {
        None | Some(PropValue::Bool(_)) => Ok(()),
        Some(other) => Err(HookError::validation(format!(
            "{field} must be a bool (got {})",
            other.type_name()
        ))),
    }
}

fn check_string_list_if_present(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
) -> Result<(), HookError> {
    let list = match obj.get(field) {
        None => return Ok(()),
        Some(PropValue::List(l)) => l,
        Some(other) => {
            return Err(HookError::validation(format!(
                "{field} must be a list (got {})",
                other.type_name()
            )));
        }
    };
    for (i, entry) in list.iter().enumerate() {
        match entry {
            PropValue::String(s) if !s.is_empty() => {}
            PropValue::String(_) => {
                return Err(HookError::validation(format!("{field}[{i}] is empty")));
            }
            other => {
                return Err(HookError::validation(format!(
                    "{field}[{i}] must be a string (got {})",
                    other.type_name()
                )));
            }
        }
    }
    Ok(())
}

/// Read a u64 field, defaulting when absent and **clamping** (not
/// erroring) an out-of-range stored value into `[min, max]` — the read
/// path is defensive so a corrupt row degrades to a safe value rather
/// than wedging the worker. A wrong *type*, however, is a hard error.
fn read_u64_or(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    min: u64,
    max: u64,
    default: u64,
) -> Result<u64> {
    let raw = match obj.get(field) {
        None => return Ok(default),
        Some(PropValue::UInt(n)) => *n,
        Some(PropValue::Int(n)) if *n >= 0 => *n as u64,
        Some(PropValue::Int(n)) => {
            anyhow::bail!("retention.{field} {n} must be non-negative");
        }
        Some(other) => {
            anyhow::bail!(
                "retention.{field} must be an unsigned integer (got {})",
                other.type_name()
            );
        }
    };
    Ok(raw.clamp(min, max))
}

fn read_bool_or(obj: &BTreeMap<String, PropValue>, field: &str, default: bool) -> Result<bool> {
    match obj.get(field) {
        None => Ok(default),
        Some(PropValue::Bool(b)) => Ok(*b),
        Some(other) => {
            anyhow::bail!(
                "retention.{field} must be a bool (got {})",
                other.type_name()
            )
        }
    }
}

/// Read a list-of-strings field, defaulting when absent. Non-string or
/// empty-string entries are dropped defensively (the read path must not
/// wedge the worker on a malformed armed-accounts row); a wrong outer
/// type is a hard error.
fn read_string_list_or(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    default: Vec<String>,
) -> Result<Vec<String>> {
    match obj.get(field) {
        None => Ok(default),
        Some(PropValue::List(l)) => Ok(l
            .iter()
            .filter_map(|v| match v {
                PropValue::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
            .collect()),
        Some(other) => {
            anyhow::bail!(
                "retention.{field} must be a list (got {})",
                other.type_name()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: Vec<(&str, PropValue)>) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v);
        }
        PropValue::Object(m)
    }

    #[test]
    fn absent_fields_read_as_inert_defaults() {
        let cfg = record_to_config(&obj(vec![])).unwrap();
        assert_eq!(cfg, RetentionConfig::default());
        assert!(cfg.is_disabled());
        assert!(cfg.dry_run);
    }

    #[test]
    fn present_fields_override_defaults() {
        let cfg = record_to_config(&obj(vec![
            ("junk_retention_days", PropValue::UInt(7)),
            ("trash_retention_days", PropValue::UInt(30)),
            ("dry_run", PropValue::Bool(false)),
            (
                "armed_accounts",
                PropValue::List(vec![PropValue::String("a@b.c".into())]),
            ),
        ]))
        .unwrap();
        assert_eq!(cfg.junk_retention_days, 7);
        assert_eq!(cfg.trash_retention_days, 30);
        assert_eq!(cfg.tick_minutes, 60); // default
        assert!(!cfg.dry_run);
        // Window set AND an account opted in ⇒ enabled.
        assert!(!cfg.is_disabled());
    }

    #[test]
    fn armed_accounts_reads_and_filters_junk() {
        // Default absent → empty (no fleet auto-default).
        assert!(
            record_to_config(&obj(vec![]))
                .unwrap()
                .armed_accounts
                .is_empty()
        );

        // A list reads through; empty/non-string entries are dropped
        // defensively rather than wedging the worker.
        let cfg = record_to_config(&obj(vec![(
            "armed_accounts",
            PropValue::List(vec![
                PropValue::String("a@b.c".into()),
                PropValue::String(String::new()), // dropped
                PropValue::UInt(5),               // dropped
                PropValue::String("d@e.f".into()),
            ]),
        )]))
        .unwrap();
        assert_eq!(
            cfg.armed_accounts,
            vec!["a@b.c".to_string(), "d@e.f".to_string()]
        );
    }

    #[test]
    fn empty_armed_accounts_is_disabled_even_with_window() {
        // is_disabled is true if EITHER no window OR no armed account.
        let cfg = record_to_config(&obj(vec![
            ("junk_retention_days", PropValue::UInt(7)),
            ("dry_run", PropValue::Bool(false)),
        ]))
        .unwrap();
        assert!(cfg.is_disabled(), "armed_accounts empty ⇒ disabled");

        let armed = record_to_config(&obj(vec![
            ("junk_retention_days", PropValue::UInt(7)),
            (
                "armed_accounts",
                PropValue::List(vec![PropValue::String("a@b.c".into())]),
            ),
        ]))
        .unwrap();
        assert!(!armed.is_disabled(), "window + an armed account ⇒ enabled");
    }

    #[test]
    fn corrupt_out_of_range_value_clamps_not_wedges() {
        // A stored days value above the ceiling clamps down rather than
        // erroring — the read path stays defensive.
        let cfg = record_to_config(&obj(vec![(
            "junk_retention_days",
            PropValue::UInt(9_999_999),
        )]))
        .unwrap();
        assert_eq!(cfg.junk_retention_days, MAX_RETENTION_DAYS);
    }

    #[test]
    fn wrong_type_is_a_hard_error() {
        let r = record_to_config(&obj(vec![(
            "junk_retention_days",
            PropValue::String("seven".into()),
        )]));
        assert!(r.is_err());
    }

    #[test]
    fn auth_policy_gates_write_on_operator_allowlist() {
        let write = Capability::from("props.write:maild.retention");
        let read = Capability::from("props.read:maild.retention");

        // An operator-named peer gets write (can arm).
        let policy = auth_policy(vec!["ops-node".to_string()]);
        let op = PeerIdentity {
            service_name: Some("ops-node".to_string()),
            ..Default::default()
        };
        let caps = policy.resolve(&op);
        assert!(caps.contains(&write), "named operator can write");
        assert!(caps.contains(&read), "everyone can read");

        // A non-operator peer can read but NOT write (can't arm).
        let other = PeerIdentity {
            service_name: Some("random-node".to_string()),
            ..Default::default()
        };
        let caps = policy.resolve(&other);
        assert!(!caps.contains(&write), "non-operator cannot write");
        assert!(caps.contains(&read), "non-operator can still read");

        // Empty allowlist (the shipped default) denies ALL remote writes.
        let none = auth_policy(Vec::new());
        assert!(
            !none.resolve(&op).contains(&write),
            "empty operators ⇒ no remote arming"
        );
    }
}
